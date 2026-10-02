mod common;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use common::{client_config, expect_bound, server_config, start_client, start_server};
use hawse_core::identity::Identity;
use hawse_proto::proxy;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// Reads one PROXY v2 header off each connection, hands it over, then echoes the rest.
async fn proxied_echo() -> (SocketAddr, mpsc::Receiver<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (headers, received) = mpsc::channel(4);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let headers = headers.clone();
            tokio::spawn(async move {
                let mut header = vec![0u8; 16];
                socket.read_exact(&mut header).await.unwrap();
                let len = usize::from(u16::from_be_bytes([header[14], header[15]]));
                header.resize(16 + len, 0);
                socket.read_exact(&mut header[16..]).await.unwrap();
                headers.send(header).await.unwrap();
                let (mut rd, mut wr) = socket.split();
                let _ = tokio::io::copy(&mut rd, &mut wr).await;
            });
        }
    });
    (addr, received)
}

async fn the_header_names_the_visitor(visitor_ip: IpAddr) {
    let server_id = Identity::generate().unwrap();
    let client_id = Identity::generate().unwrap();
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[])]),
        &server_id,
    );
    let (local, mut headers) = proxied_echo().await;
    let mut cfg = client_config(
        server.addr,
        server.key,
        &[("proxied", &local.to_string(), "any")],
    );
    cfg.expose.get_mut("proxied").unwrap().proxy_protocol = true;
    let mut client = start_client(cfg, client_id);
    let port = expect_bound(&mut client.events, "proxied").await.number;

    let mut visitor = TcpStream::connect((visitor_ip, port)).await.unwrap();
    visitor.write_all(b"hello").await.unwrap();
    let mut buf = [0u8; 5];
    visitor.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"hello");

    let header = tokio::time::timeout(Duration::from_secs(5), headers.recv())
        .await
        .expect("a header within 5 s")
        .unwrap();
    let expected = proxy::v2_tcp(
        visitor.local_addr().unwrap(),
        SocketAddr::new(visitor_ip, port),
    );
    assert_eq!(header, expected);

    client.cancel.cancel();
    server.cancel.cancel();
}

#[tokio::test]
async fn a_proxied_service_reads_an_ipv4_visitor_before_its_bytes() {
    the_header_names_the_visitor(Ipv4Addr::LOCALHOST.into()).await;
}

#[tokio::test]
async fn a_proxied_service_reads_an_ipv6_visitor_before_its_bytes() {
    the_header_names_the_visitor(Ipv6Addr::LOCALHOST.into()).await;
}
