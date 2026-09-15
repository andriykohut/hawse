mod common;

use hawse_core::pump::pump;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn pattern(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x & 0xff) as u8
        })
        .collect()
}

#[tokio::test]
async fn half_close_lets_the_other_direction_finish() {
    let pair = common::quic_pair().await;
    let (send, recv) = pair.client.open_bi().await.unwrap();
    let (near, far) = tokio::io::duplex(64 * 1024);
    let pumped = tokio::spawn(pump(near, send, recv, 16 * 1024));
    let (mut far_rd, mut far_wr) = tokio::io::split(far);

    far_wr.write_all(b"ping").await.unwrap();
    far_wr.shutdown().await.unwrap();

    let (mut peer_send, mut peer_recv) = pair.server.accept_bi().await.unwrap();
    assert_eq!(peer_recv.read_to_end(64).await.unwrap(), b"ping");

    peer_send.write_all(b"pong").await.unwrap();
    peer_send.finish().unwrap();
    let mut got = Vec::new();
    far_rd.read_to_end(&mut got).await.unwrap();
    assert_eq!(got, b"pong");

    let stats = pumped.await.unwrap().unwrap();
    assert_eq!((stats.to_stream, stats.to_socket), (4, 4));
}

#[tokio::test]
async fn moves_large_payloads_intact_both_ways() {
    const LEN: usize = 8 * 1024 * 1024;
    let pair = common::quic_pair().await;
    let (send, recv) = pair.client.open_bi().await.unwrap();
    let (near, far) = tokio::io::duplex(256 * 1024);
    let pumped = tokio::spawn(pump(near, send, recv, 16 * 1024));
    let (mut far_rd, mut far_wr) = tokio::io::split(far);

    let up = pattern(LEN, 1);
    let down = pattern(LEN, 2);
    let up_expected = up.clone();
    let down_expected = down.clone();

    // Started before accept_bi: quinn only reveals a stream to the peer once a frame carries
    // data for it, so accept_bi would hang forever waiting on a stream nothing has written to.
    let writer_far = tokio::spawn(async move {
        far_wr.write_all(&up).await.unwrap();
        far_wr.shutdown().await.unwrap();
    });
    let reader_far = tokio::spawn(async move {
        let mut got = Vec::with_capacity(LEN);
        far_rd.read_to_end(&mut got).await.unwrap();
        got
    });

    let (mut peer_send, mut peer_recv) = pair.server.accept_bi().await.unwrap();
    let writer_peer = tokio::spawn(async move {
        peer_send.write_all(&down).await.unwrap();
        peer_send.finish().unwrap();
        peer_send.stopped().await.ok();
    });
    let reader_peer = tokio::spawn(async move { peer_recv.read_to_end(LEN).await.unwrap() });

    writer_far.await.unwrap();
    writer_peer.await.unwrap();
    assert_eq!(reader_far.await.unwrap(), down_expected);
    assert_eq!(reader_peer.await.unwrap(), up_expected);
    let stats = pumped.await.unwrap().unwrap();
    assert_eq!((stats.to_stream, stats.to_socket), (LEN as u64, LEN as u64));
}

#[tokio::test]
async fn frames_round_trip_on_raw_streams() {
    use hawse_core::frame::{read_frame, write_frame};
    use hawse_proto::msg::StreamHeader;
    let pair = common::quic_pair().await;
    let (mut send, _recv) = pair.client.open_bi().await.unwrap();
    let header = StreamHeader {
        service_id: 9,
        visitor: "203.0.113.5:5555".parse().unwrap(),
        listener: "[::]:443".parse().unwrap(),
    };
    write_frame(&mut send, &header).await.unwrap();
    send.write_all(b"payload").await.unwrap();
    send.finish().unwrap();
    let (_send, mut recv) = pair.server.accept_bi().await.unwrap();
    assert_eq!(read_frame::<StreamHeader>(&mut recv).await.unwrap(), header);
    assert_eq!(recv.read_to_end(64).await.unwrap(), b"payload");
}
