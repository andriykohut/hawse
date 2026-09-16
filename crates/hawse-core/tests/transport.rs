mod common;

use std::time::Duration;

use bytes::Bytes;
use futures_util::FutureExt;
use hawse_core::transport::CloseReason;
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
    timeout(Duration::from_secs(5), sr.read_to_end(&mut got))
        .await
        .expect("the peer never saw end-of-stream")
        .unwrap();
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
async fn resetting_a_send_half_fails_the_peers_read_and_carries_the_code() {
    const CODE: u32 = 0x42;

    let pair = common::quic_pair().await;
    let client = pair.client_transport();
    let server = pair.server_transport();

    let (mut cs, _cr) = client.open_bi().await.unwrap();
    cs.write_all(b"hi").await.unwrap();
    let (_ss, mut sr) = server.accept_bi().await.unwrap();
    cs.reset(CODE);

    let mut got = Vec::new();
    let read = timeout(Duration::from_secs(5), sr.read_to_end(&mut got))
        .await
        .expect("the peer never saw the reset");
    let err = read.expect_err("a reset stream must fail the read, not end it cleanly");
    assert_eq!(
        err.get_ref()
            .and_then(|e| e.downcast_ref::<quinn::ReadError>()),
        Some(&quinn::ReadError::Reset(quinn::VarInt::from_u32(CODE))),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_datagram_reaches_the_peer() {
    let pair = common::quic_pair().await;
    let client = pair.client_transport();
    let server = pair.server_transport();

    client.send_datagram(Bytes::from_static(b"ping")).unwrap();
    let got = timeout(Duration::from_secs(5), server.recv_datagram())
        .await
        .expect("no datagram arrived")
        .unwrap();
    assert_eq!(got, Bytes::from_static(b"ping"));
}

#[tokio::test]
async fn quic_transport_reports_a_datagram_size() {
    let pair = common::quic_pair().await;
    assert!(pair.client_transport().max_datagram_size().is_some());
}

#[tokio::test]
async fn closed_resolves_only_once_the_peer_has_closed() {
    let pair = common::quic_pair().await;
    let client = pair.client_transport();
    let server = pair.server_transport();

    assert!(
        server.closed().now_or_never().is_none(),
        "closed() resolved on a live connection"
    );

    client.close(CloseReason::Shutdown);
    timeout(Duration::from_secs(5), server.closed())
        .await
        .expect("the peer never saw the close");
}
