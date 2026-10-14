mod common;

use std::time::Duration;

use common::{client_config, echo_server, expect_bound, server_config, start, start_server};
use hawse_core::client::Client;
use hawse_core::identity::Identity;
use hawse_core::traffic::{Counts, Traffic};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn ids() -> (Identity, Identity) {
    (Identity::generate().unwrap(), Identity::generate().unwrap())
}

/// Waits for `service`'s totals to read `want`: a pump adds as it copies, so the figures can
/// trail the bytes a test has read by a moment.
async fn settles(traffic: &Traffic, service: &str, want: Counts) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let got = traffic.snapshot().get(service).copied().unwrap_or_default();
        if got == want {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{service}: {got:?}, not {want:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn a_visitor_is_counted_open_while_it_is_and_its_bytes_each_way() {
    let (server_id, client_id) = ids();
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[])]),
        &server_id,
    );
    let echo = echo_server().await.to_string();
    let client = Client::new(
        client_config(server.addr, server.key, &[("echo", &echo, "any")]),
        client_id,
    );
    let traffic = client.traffic();
    let mut client = start(client);
    let port = expect_bound(&mut client.events, "echo").await;

    let mut visitor = TcpStream::connect(("127.0.0.1", port.number))
        .await
        .unwrap();
    visitor.write_all(&[7u8; 1000]).await.unwrap();
    let mut back = [0u8; 1000];
    visitor.read_exact(&mut back).await.unwrap();
    let open = Counts {
        open: 1,
        down: 1000,
        up: 1000,
    };
    settles(&traffic, "echo", open).await;
    drop(visitor);
    settles(&traffic, "echo", Counts { open: 0, ..open }).await;

    client.cancel.cancel();
    server.cancel.cancel();
}

#[tokio::test]
async fn a_visitor_local_refuses_is_never_counted_open() {
    let (server_id, client_id) = ids();
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[])]),
        &server_id,
    );
    // A port nothing listens on any more.
    let closed = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .to_string();
    let client = Client::new(
        client_config(server.addr, server.key, &[("dead", &closed, "any")]),
        client_id,
    );
    let traffic = client.traffic();
    let mut client = start(client);
    let port = expect_bound(&mut client.events, "dead").await;

    let mut visitor = TcpStream::connect(("127.0.0.1", port.number))
        .await
        .unwrap();
    let mut buf = [0u8; 16];
    // The refusal ends the visitor's connection, cleanly or with a reset.
    let _ = tokio::time::timeout(Duration::from_secs(5), visitor.read(&mut buf))
        .await
        .expect("the visitor is let go within 5 s");
    assert!(!traffic.take_moved(), "a refused dial was counted");
    assert_eq!(traffic.snapshot()["dead"], Counts::default());

    client.cancel.cancel();
    server.cancel.cancel();
}
