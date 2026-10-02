mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::{
    client_config, echo_server, expect_bound, next_event, server_config, start_client,
    start_server, udp_ask, udp_echo_server, udp_recv, udp_visitor,
};
use hawse_core::client::Event;
use hawse_core::identity::Identity;
use hawse_proto::msg::BindFailure;
use ipnet::IpNet;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// Every server here keeps `bind` at its default `::`, a dual-stack socket that reports a visitor
// from 127.0.0.1 as ::ffff:127.0.0.1. Setting `bind` would skip that path.

fn ids() -> (Identity, Identity) {
    (Identity::generate().unwrap(), Identity::generate().unwrap())
}

fn nets(list: &[&str]) -> Vec<IpNet> {
    list.iter().map(|net| net.parse().unwrap()).collect()
}

/// Counts the connections it accepts and answers none of them.
async fn counting_service() -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicUsize::new(0));
    tokio::spawn({
        let accepted = Arc::clone(&accepted);
        async move {
            while listener.accept().await.is_ok() {
                accepted.fetch_add(1, Ordering::Relaxed);
            }
        }
    });
    (addr, accepted)
}

/// Whether a visitor from 127.0.0.1 gets its bytes echoed back through `port`.
async fn echoed(port: u16) -> bool {
    let Ok(mut visitor) = TcpStream::connect(("127.0.0.1", port)).await else {
        return false;
    };
    if visitor.write_all(b"hello").await.is_err() {
        return false;
    }
    let mut buf = [0u8; 5];
    let read = tokio::time::timeout(Duration::from_secs(2), visitor.read_exact(&mut buf)).await;
    matches!(read, Ok(Ok(_))) && buf == *b"hello"
}

#[tokio::test]
async fn a_tcp_visitor_outside_the_list_never_reaches_the_service() {
    let (server_id, client_id) = ids();
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[])]),
        &server_id,
    );
    let echo = echo_server().await.to_string();
    let (counted, accepted) = counting_service().await;
    let mut cfg = client_config(
        server.addr,
        server.key,
        &[
            ("inside", &echo, "any"),
            ("outside", &counted.to_string(), "any"),
        ],
    );
    cfg.expose.get_mut("inside").unwrap().allow = nets(&["127.0.0.1/32"]);
    cfg.expose.get_mut("outside").unwrap().allow = nets(&["192.0.2.0/24"]);
    let mut client = start_client(cfg, client_id);
    let inside = expect_bound(&mut client.events, "inside").await;
    let outside = expect_bound(&mut client.events, "outside").await;

    assert!(echoed(inside.number).await);
    assert!(!echoed(outside.number).await);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(accepted.load(Ordering::Relaxed), 0);

    client.cancel.cancel();
    server.cancel.cancel();
}

#[tokio::test]
async fn a_ceiling_applies_to_an_unlisted_service_and_refuses_a_disjoint_one() {
    let (server_id, client_id) = ids();
    let mut server_cfg = server_config(&[("test", client_id.public_key(), &[])]);
    server_cfg.clients.get_mut("test").unwrap().allow = nets(&["192.0.2.0/24"]);
    let server = start_server(&server_cfg, &server_id);
    let (counted, accepted) = counting_service().await;
    let counted = counted.to_string();
    let mut cfg = client_config(
        server.addr,
        server.key,
        &[("bare", &counted, "any"), ("disjoint", &counted, "any")],
    );
    cfg.expose.get_mut("disjoint").unwrap().allow = nets(&["198.51.100.0/24"]);
    let mut client = start_client(cfg, client_id);

    let bare = expect_bound(&mut client.events, "bare").await;
    loop {
        match next_event(&mut client.events).await {
            Event::BindFailed { service, reason } if service == "disjoint" => {
                assert_eq!(reason, BindFailure::NotGranted);
                break;
            }
            Event::Bound { service, port } if service == "disjoint" => {
                panic!("disjoint was bound on {port}")
            }
            _ => {}
        }
    }
    assert!(!echoed(bare.number).await);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(accepted.load(Ordering::Relaxed), 0);

    client.cancel.cancel();
    server.cancel.cancel();
}

#[tokio::test]
async fn a_ceiling_narrows_a_client_list_to_what_both_allow() {
    let (server_id, client_id) = ids();
    let mut server_cfg = server_config(&[("test", client_id.public_key(), &[])]);
    server_cfg.clients.get_mut("test").unwrap().allow = nets(&["127.0.0.1/32"]);
    let server = start_server(&server_cfg, &server_id);
    let echo = echo_server().await.to_string();
    let mut cfg = client_config(server.addr, server.key, &[("echo", &echo, "any")]);
    cfg.expose.get_mut("echo").unwrap().allow = nets(&["127.0.0.0/8", "192.0.2.0/24"]);
    let mut client = start_client(cfg, client_id);

    let port = expect_bound(&mut client.events, "echo").await;
    assert!(echoed(port.number).await);

    client.cancel.cancel();
    server.cancel.cancel();
}

#[tokio::test]
async fn a_udp_visitor_outside_the_list_gets_no_reply() {
    let (server_id, client_id) = ids();
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[])]),
        &server_id,
    );
    let echo = udp_echo_server().await.to_string();
    let mut cfg = client_config(
        server.addr,
        server.key,
        &[("inside", &echo, "any/udp"), ("outside", &echo, "any/udp")],
    );
    cfg.expose.get_mut("inside").unwrap().allow = nets(&["127.0.0.0/8"]);
    cfg.expose.get_mut("outside").unwrap().allow = nets(&["192.0.2.0/24"]);
    let mut client = start_client(cfg, client_id);
    let inside = expect_bound(&mut client.events, "inside").await;
    let outside = expect_bound(&mut client.events, "outside").await;

    assert_eq!(
        udp_ask(&udp_visitor().await, inside.number, b"hello").await,
        b"hello"
    );
    // Its own socket, so a late duplicate from the `inside` exchange cannot answer for it.
    let stranger = udp_visitor().await;
    for _ in 0..3 {
        stranger
            .send_to(b"hello", ("127.0.0.1", outside.number))
            .await
            .unwrap();
    }
    assert_eq!(udp_recv(&stranger).await, None);

    client.cancel.cancel();
    server.cancel.cancel();
}
