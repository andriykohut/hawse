mod common;

use std::time::Duration;

use common::{
    DYNAMIC_PORTS, free_udp_port_outside_pool, raw_client, server_config, start_server, udp_recv,
    udp_visitor,
};
use hawse_core::config::{Limits, ServerConfig};
use hawse_core::frame::{read_body, write_body};
use hawse_core::identity::Identity;
use hawse_proto::msg::{BindFailure, ClientMessage, DatagramHeader, ServerMessage};
use hawse_proto::packet;
use hawse_proto::port::Kind;

fn ids() -> (Identity, Identity) {
    (Identity::generate().unwrap(), Identity::generate().unwrap())
}

async fn datagram(client: &common::RawClient) -> (DatagramHeader, Vec<u8>) {
    let packet = tokio::time::timeout(Duration::from_secs(2), client.conn.read_datagram())
        .await
        .expect("a datagram within 2 s")
        .unwrap();
    let (header, payload) = packet::decode(&packet).unwrap();
    (header, payload.to_vec())
}

#[tokio::test]
async fn datagrams_cross_a_udp_service_both_ways() {
    let (server_id, client_id) = ids();
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[])]),
        &server_id,
    );
    let mut client = raw_client(&server, &client_id).await;
    let (id, port) = client.bound_udp("dns").await;
    assert!(DYNAMIC_PORTS.contains(&port));
    let (bulk_id, _send, _recv) = client.accept_bulk().await;
    assert_eq!(bulk_id, id);

    let visitor = udp_visitor().await;
    visitor
        .send_to(b"query", ("127.0.0.1", port))
        .await
        .unwrap();
    let (header, payload) = datagram(&client).await;
    assert_eq!((header.service_id, &payload[..]), (id, &b"query"[..]));

    client
        .conn
        .send_datagram(packet::encode(header, b"answer").unwrap())
        .unwrap();
    assert_eq!(udp_recv(&visitor).await.as_deref(), Some(&b"answer"[..]));
    server.cancel.cancel();
}

#[tokio::test]
async fn a_payload_no_datagram_holds_takes_the_bulk_stream_both_ways() {
    let (server_id, client_id) = ids();
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[])]),
        &server_id,
    );
    let mut client = raw_client(&server, &client_id).await;
    let (id, port) = client.bound_udp("big").await;
    let (_, mut send, mut recv) = client.accept_bulk().await;

    let visitor = udp_visitor().await;
    let question = vec![1u8; 4000];
    visitor
        .send_to(&question, ("127.0.0.1", port))
        .await
        .unwrap();
    let body = tokio::time::timeout(Duration::from_secs(2), read_body(&mut recv))
        .await
        .expect("a bulk frame within 2 s")
        .unwrap();
    let (header, payload) = packet::decode(&body).unwrap();
    assert_eq!((header.service_id, payload), (id, &question[..]));

    let answer = vec![2u8; 4000];
    write_body(&mut send, &packet::encode(header, &answer).unwrap())
        .await
        .unwrap();
    assert_eq!(udp_recv(&visitor).await, Some(answer));
    server.cancel.cancel();
}

#[tokio::test]
async fn a_reply_for_a_session_nobody_holds_reaches_no_visitor() {
    let (server_id, client_id) = ids();
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[])]),
        &server_id,
    );
    let mut client = raw_client(&server, &client_id).await;
    let (id, port) = client.bound_udp("dns").await;

    let visitor = udp_visitor().await;
    visitor
        .send_to(b"query", ("127.0.0.1", port))
        .await
        .unwrap();
    let (header, _) = datagram(&client).await;
    let stranger = DatagramHeader {
        service_id: id,
        session: header.session.wrapping_add(1000),
    };
    client
        .conn
        .send_datagram(packet::encode(stranger, b"answer").unwrap())
        .unwrap();

    assert_eq!(udp_recv(&visitor).await, None);
    server.cancel.cancel();
}

#[tokio::test]
async fn a_visitor_evicted_at_the_cap_stops_receiving_replies() {
    let (server_id, client_id) = ids();
    let cfg = ServerConfig {
        limits: Limits {
            udp_sessions_per_service: 1,
            ..Limits::default()
        },
        ..server_config(&[("test", client_id.public_key(), &[])])
    };
    let server = start_server(&cfg, &server_id);
    let mut client = raw_client(&server, &client_id).await;
    let (_, port) = client.bound_udp("dns").await;

    let first = udp_visitor().await;
    first.send_to(b"one", ("127.0.0.1", port)).await.unwrap();
    let (evicted, _) = datagram(&client).await;
    let second = udp_visitor().await;
    second.send_to(b"two", ("127.0.0.1", port)).await.unwrap();
    let (kept, _) = datagram(&client).await;

    for header in [evicted, kept] {
        client
            .conn
            .send_datagram(packet::encode(header, b"answer").unwrap())
            .unwrap();
    }
    assert_eq!(udp_recv(&second).await.as_deref(), Some(&b"answer"[..]));
    assert_eq!(udp_recv(&first).await, None);
    server.cancel.cancel();
}

#[tokio::test]
async fn a_fixed_udp_port_is_free_again_once_its_session_ends() {
    let (server_id, client_id) = ids();
    let port = free_udp_port_outside_pool().await;
    let grant = format!("{port}/udp");
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[&grant])]),
        &server_id,
    );

    let mut first = raw_client(&server, &client_id).await;
    assert!(matches!(
        first.bind_udp("wg", Some(port)).await,
        ServerMessage::Bound { .. }
    ));
    first.conn.close(0u32.into(), b"");

    let mut second = raw_client(&server, &client_id).await;
    assert!(matches!(
        second.bind_udp("wg", Some(port)).await,
        ServerMessage::Bound { port: bound, .. } if bound == port
    ));
    server.cancel.cancel();
}

#[tokio::test]
async fn a_udp_bind_with_an_allow_list_is_still_refused() {
    let (server_id, client_id) = ids();
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[])]),
        &server_id,
    );
    let mut client = raw_client(&server, &client_id).await;
    client
        .control
        .send(&ClientMessage::Bind {
            service: "dns".to_owned(),
            kind: Kind::Udp,
            port: None,
            allow: vec!["203.0.113.0/24".parse().unwrap()],
            proxy_protocol: false,
        })
        .await
        .unwrap();
    assert_eq!(
        client.reply().await,
        ServerMessage::BindFailed {
            service: "dns".to_owned(),
            reason: BindFailure::Unsupported,
        }
    );
    server.cancel.cancel();
}
