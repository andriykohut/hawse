mod common;

use std::time::Duration;

use common::{
    client_config, echo_server, expect_bound, fixed_port, raw_client, raw_hello, server_config,
    start, start_server, udp_ask, udp_echo_server, udp_replier, udp_visitor,
};
use hawse_core::client::Client;
use hawse_core::identity::Identity;
use hawse_core::server::policy::Policy;
use hawse_proto::msg::{BindFailure, ServerMessage};
use hawse_proto::port::Kind;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn echoes(port: u16) {
    let mut visitor = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    visitor.write_all(b"hello").await.unwrap();
    let mut buf = [0u8; 5];
    visitor.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"hello");
}

#[tokio::test]
async fn a_client_a_reload_no_longer_names_is_told_and_then_denied() {
    let (server_id, client_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[])]),
        &server_id,
    );
    let mut client = raw_client(&server, &client_id).await;

    server
        .policy
        .send_replace(Policy::from_config(&server_config(&[])));

    let ServerMessage::Shutdown { reason } = client.reply().await else {
        panic!("a revoked session was not told it is over");
    };
    assert!(reason.contains("no longer authorized"), "{reason}");
    let mut again = raw_hello(&server, &client_id).await;
    assert!(matches!(again.reply().await, ServerMessage::Denied { .. }));
    server.cancel.cancel();
}

#[tokio::test]
async fn a_port_a_reload_takes_out_of_a_grant_is_unbound_and_the_session_goes_on() {
    let (server_id, client_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let granted = fixed_port(Kind::Udp);
    let grant = format!("{granted}/udp");
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[&grant])]),
        &server_id,
    );
    let mut client = raw_client(&server, &client_id).await;
    assert!(matches!(
        client.bind_udp("fixed", Some(granted)).await,
        ServerMessage::Bound { .. }
    ));
    client.bound_udp("dynamic").await;

    server
        .policy
        .send_replace(Policy::from_config(&server_config(&[(
            "test",
            client_id.public_key(),
            &[],
        )])));

    let lost = ServerMessage::BindFailed {
        service: "fixed".to_owned(),
        reason: BindFailure::NotGranted,
    };
    assert_eq!(client.reply().await, lost);
    // The name is free again, the port is not this client's to ask for, and the dynamic service
    // was never under the grant.
    assert_eq!(client.bind_udp("fixed", Some(granted)).await, lost);
    assert_eq!(
        client.bind_udp("dynamic", None).await,
        ServerMessage::BindFailed {
            service: "dynamic".to_owned(),
            reason: BindFailure::InUse,
        }
    );
    server.cancel.cancel();
}

#[tokio::test]
async fn a_running_client_binds_a_service_a_reload_adds_and_unbinds_one_it_removes() {
    let (server_id, client_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[])]),
        &server_id,
    );
    let echo = echo_server().await.to_string();
    let one = client_config(server.addr, server.key, &[("one", &echo, "any")]);
    let both = client_config(
        server.addr,
        server.key,
        &[("one", &echo, "any"), ("two", &echo, "any")],
    );
    let two = client_config(server.addr, server.key, &[("two", &echo, "any")]);

    let client = Client::new(one, client_id);
    let expose = client.expose();
    let mut client = start(client);
    let first = expect_bound(&mut client.events, "one").await.number;
    echoes(first).await;

    expose.send_replace(both.expose);
    let second = expect_bound(&mut client.events, "two").await.number;
    echoes(second).await;
    echoes(first).await;

    expose.send_replace(two.expose);
    let closed = async {
        while TcpStream::connect(("127.0.0.1", first)).await.is_ok() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), closed)
        .await
        .expect("the removed service's port closes within 5 s");
    echoes(second).await;

    client.cancel.cancel();
    assert!(client.task.await.unwrap().is_ok());
    server.cancel.cancel();
}

#[tokio::test]
async fn a_service_a_reload_changes_is_bound_again_on_its_fixed_port() {
    let (server_id, client_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let port = fixed_port(Kind::Tcp).to_string();
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[&port])]),
        &server_id,
    );
    let echo = echo_server().await.to_string();
    let moved = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let moved_addr = moved.local_addr().unwrap().to_string();
    let before = client_config(server.addr, server.key, &[("web", &echo, &port)]);
    let after = client_config(server.addr, server.key, &[("web", &moved_addr, &port)]);

    let client = Client::new(before, client_id);
    let expose = client.expose();
    let mut client = start(client);
    let bound = expect_bound(&mut client.events, "web").await.number;
    echoes(bound).await;

    // The unbind and the bind for the same port reach the server back to back.
    expose.send_replace(after.expose);
    assert_eq!(expect_bound(&mut client.events, "web").await.number, bound);
    let _visitor = TcpStream::connect(("127.0.0.1", bound)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), moved.accept())
        .await
        .expect("the changed service reaches its new local address within 5 s")
        .unwrap();

    client.cancel.cancel();
    server.cancel.cancel();
}

#[tokio::test]
async fn a_udp_service_a_reload_changes_is_bound_again_on_its_fixed_port() {
    let (server_id, client_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let port = format!("{}/udp", fixed_port(Kind::Udp));
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[&port])]),
        &server_id,
    );
    let echo = udp_echo_server().await.to_string();
    let moved = udp_replier(3).await.to_string();
    let before = client_config(server.addr, server.key, &[("dns", &echo, &port)]);
    let after = client_config(server.addr, server.key, &[("dns", &moved, &port)]);

    let client = Client::new(before, client_id);
    let expose = client.expose();
    let mut client = start(client);
    let bound = expect_bound(&mut client.events, "dns").await.number;
    let visitor = udp_visitor().await;
    assert_eq!(udp_ask(&visitor, bound, b"hello").await, b"hello");

    expose.send_replace(after.expose);
    assert_eq!(expect_bound(&mut client.events, "dns").await.number, bound);
    assert_eq!(udp_ask(&visitor, bound, b"hello").await, [0xAB; 3]);

    client.cancel.cancel();
    server.cancel.cancel();
}

#[tokio::test]
async fn an_answer_to_a_bind_a_reload_has_replaced_binds_nothing() {
    use futures_util::{SinkExt, StreamExt};
    use hawse_core::frame::write_frame;
    use hawse_core::tls;
    use hawse_core::transport::SendHalf;
    use hawse_core::transport::quic::{self, Tuning};
    use hawse_proto::frame::{codec, decode, encode};
    use hawse_proto::msg::{ClientMessage, StreamHeader, StreamOpen};
    use tokio_util::codec::{FramedRead, FramedWrite};

    let (server_id, client_id) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let (cert, key) = server_id.certificate().unwrap();
    let slow = quic::listen(
        "127.0.0.1:0".parse().unwrap(),
        tls::server_config(cert, key, tls::provider()).unwrap(),
        Tuning::SERVER,
    )
    .unwrap();
    let addr = slow.local_addr().unwrap();
    let old = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let new = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let old_addr = old.local_addr().unwrap().to_string();
    let new_addr = new.local_addr().unwrap().to_string();
    let before = client_config(addr, server_id.public_key(), &[("svc", &old_addr, "any")]);
    let after = client_config(addr, server_id.public_key(), &[("svc", &new_addr, "any")]);

    let client = Client::new(before, client_id);
    let expose = client.expose();
    let mut client = start(client);

    let conn = slow.accept().await.unwrap().await.unwrap();
    let (send, recv) = conn.accept_bi().await.unwrap();
    let mut tx = FramedWrite::new(send, codec());
    let mut rx = FramedRead::new(recv, codec());
    let mut heard = async || loop {
        let msg = decode::<ClientMessage>(&rx.next().await.unwrap().unwrap()).unwrap();
        if !matches!(msg, ClientMessage::Ping { .. } | ClientMessage::Pong { .. }) {
            return msg;
        }
    };
    let bound = |service_id| {
        encode(&ServerMessage::Bound {
            service: "svc".into(),
            service_id,
            port: 40000,
            address: std::net::Ipv4Addr::LOCALHOST.into(),
        })
        .unwrap()
    };
    assert!(matches!(heard().await, ClientMessage::Hello { .. }));
    let welcome = ServerMessage::Welcome {
        agent: "slow".into(),
        client_name: "test".into(),
    };
    tx.send(encode(&welcome).unwrap()).await.unwrap();
    // The first bind goes unanswered until the reload's unbind and bind have come in after it.
    assert!(matches!(heard().await, ClientMessage::Bind { .. }));
    expose.send_replace(after.expose);
    assert!(matches!(heard().await, ClientMessage::Unbind { .. }));
    assert!(matches!(heard().await, ClientMessage::Bind { .. }));

    let visit = async |service_id| {
        let (send, recv) = conn.open_bi().await.unwrap();
        let mut send = SendHalf::Quic(send);
        let header = StreamHeader {
            service_id,
            visitor: "203.0.113.9:1".parse().unwrap(),
            listener: "203.0.113.1:40000".parse().unwrap(),
        };
        write_frame(&mut send, &StreamOpen::Visitor(header))
            .await
            .unwrap();
        (send, recv)
    };
    // The id of the first bind was given under the old settings, whose visitors the old `allow`
    // admitted. They reach neither address.
    tx.send(bound(1)).await.unwrap();
    let _first = visit(1).await;
    let dialed = async {
        tokio::select! {
            _ = old.accept() => "the old",
            _ = new.accept() => "the new",
        }
    };
    let dialed = tokio::time::timeout(Duration::from_millis(500), dialed).await;
    assert!(
        dialed.is_err(),
        "a visitor of the replaced bind reached {dialed:?} address"
    );

    tx.send(bound(2)).await.unwrap();
    expect_bound(&mut client.events, "svc").await;
    let _second = visit(2).await;
    tokio::time::timeout(Duration::from_secs(2), new.accept())
        .await
        .expect("a visitor of the bind in force reaches the new address")
        .unwrap();
    client.cancel.cancel();
}
