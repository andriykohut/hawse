mod common;

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

#[tokio::test]
async fn quic_transport_opens_a_bidirectional_stream() {
    let pair = common::quic_pair().await;
    let client = pair.client_transport();
    let server = pair.server_transport();

    let (mut cs, _cr) = client.open_bi().await.unwrap();
    cs.write_all(b"hi").await.unwrap();
    cs.finish().await;

    let (_ss, mut sr) = server.accept_bi().await.unwrap();
    let mut got = Vec::new();
    sr.read_to_end(&mut got).await.unwrap();
    assert_eq!(got, b"hi");
}

#[tokio::test]
async fn shutting_down_a_send_half_gives_the_peer_eof() {
    let pair = common::quic_pair().await;
    let client = pair.client_transport();
    let server = pair.server_transport();

    let (mut cs, _cr) = client.open_bi().await.unwrap();
    cs.write_all(b"hi").await.unwrap();
    cs.shutdown().await.unwrap();

    let (_ss, mut sr) = server.accept_bi().await.unwrap();
    let mut got = Vec::new();
    timeout(Duration::from_secs(5), sr.read_to_end(&mut got))
        .await
        .expect("the peer never saw end-of-stream")
        .unwrap();
    assert_eq!(got, b"hi");
}

#[tokio::test]
async fn quic_transport_reports_a_datagram_size() {
    let pair = common::quic_pair().await;
    assert!(pair.client_transport().max_datagram_size().is_some());
}
