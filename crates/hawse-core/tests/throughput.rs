mod common;

use std::time::Instant;

use hawse_core::pump::pump;
use hawse_core::transport::{RecvHalf, SendHalf, Transport};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PAYLOAD: usize = 256 * 1024 * 1024;

#[allow(clippy::cast_precision_loss)]
fn report(direction: &str, secs: f64) {
    eprintln!(
        "throughput_{direction} {:.0} MiB/s",
        (PAYLOAD as f64 / (1024.0 * 1024.0)) / secs
    );
}

// One test rather than two because cargo runs test functions in parallel: as separate tests the
// two transfers would race over the same loopback and measure contention, not throughput.
#[tokio::test]
#[ignore = "benchmark, run explicitly with --ignored --nocapture"]
async fn throughput_over_quic_in_both_directions() {
    socket_to_stream().await;
    stream_to_socket().await;
    eprintln!(
        "the two figures stop their clocks differently; compare each only against its own history"
    );
}

#[allow(clippy::similar_names)]
async fn socket_to_stream() {
    let pair = common::quic_pair().await;
    let (send, recv) = pair.client.open_bi().await.unwrap();
    let (near, far) = tokio::io::duplex(4 * 1024 * 1024);
    let pumped = tokio::spawn(pump(
        near,
        SendHalf::Quic(send),
        RecvHalf::Quic(recv),
        16 * 1024,
    ));
    let (mut far_rd, mut far_wr) = tokio::io::split(far);

    // Started before accept_bi: quinn only reveals a stream to the peer once a frame carries
    // data for it, so accept_bi would hang forever waiting on a stream nothing has written to.
    let writer = tokio::spawn(async move {
        let chunk = vec![0x5a_u8; 1 << 20];
        let mut sent = 0usize;
        while sent < PAYLOAD {
            far_wr.write_all(&chunk).await.unwrap();
            sent += chunk.len();
        }
        far_wr.shutdown().await.unwrap();
    });

    let (mut peer_send, mut peer_recv) = pair.server.accept_bi().await.unwrap();
    let arrived = tokio::spawn(async move {
        let mut total = 0usize;
        while let Ok(Some(chunk)) = peer_recv.read_chunk(1 << 20, true).await {
            total += chunk.bytes.len();
        }
        total
    });

    let started = Instant::now();
    // Only this direction is measured; the reverse would otherwise compete for the link.
    peer_send.finish().unwrap();
    let mut drain = vec![0u8; 1 << 20];
    while far_rd.read(&mut drain).await.unwrap_or(0) > 0 {}
    writer.await.unwrap();
    let secs = started.elapsed().as_secs_f64();
    let _ = pumped.await;
    assert_eq!(arrived.await.unwrap(), PAYLOAD);
    report("socket_to_stream", secs);
}

// Reading into a reused buffer measures the same here as quinn's zero-copy read_chunk: medians of
// 236 and 237 MiB/s, over spreads that overlap.
#[allow(clippy::similar_names)]
async fn stream_to_socket() {
    let pair = common::quic_pair().await;
    let (send, recv) = pair.client.open_bi().await.unwrap();
    let (near, far) = tokio::io::duplex(4 * 1024 * 1024);
    let pumped = tokio::spawn(pump(
        near,
        SendHalf::Quic(send),
        RecvHalf::Quic(recv),
        16 * 1024,
    ));
    let (mut far_rd, mut far_wr) = tokio::io::split(far);

    // quinn only reveals a stream to the peer once a frame carries data for it, and here the peer
    // is the one with a payload to send, so the pump has to put a byte on the wire first.
    far_wr.write_all(b"x").await.unwrap();
    far_wr.shutdown().await.unwrap();

    let (mut peer_send, mut peer_recv) = pair.server.accept_bi().await.unwrap();
    tokio::spawn(
        async move { while let Ok(Some(_)) = peer_recv.read_chunk(1 << 20, true).await {} },
    );

    let started = Instant::now();
    let writer = tokio::spawn(async move {
        let chunk = vec![0x5a_u8; 1 << 20];
        let mut sent = 0usize;
        while sent < PAYLOAD {
            peer_send.write_all(&chunk).await.unwrap();
            sent += chunk.len();
        }
        peer_send.finish().unwrap();
    });

    let mut drain = vec![0u8; 1 << 20];
    let mut arrived = 0usize;
    loop {
        let n = far_rd.read(&mut drain).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        arrived += n;
    }
    let secs = started.elapsed().as_secs_f64();
    writer.await.unwrap();
    let _ = pumped.await;
    assert_eq!(arrived, PAYLOAD);
    report("stream_to_socket", secs);
}

// TCP is new, so there is no baseline to defend: this records where the fallback starts.
#[tokio::test]
#[ignore = "benchmark, run explicitly with --ignored --nocapture"]
async fn throughput_over_tcp_in_both_directions() {
    let (client, server) = common::tcp_pair().await;
    report(
        "tcp_socket_to_stream",
        tcp_socket_to_stream(&*client, &*server).await,
    );
    let (client, server) = common::tcp_pair().await;
    report(
        "tcp_stream_to_socket",
        tcp_stream_to_socket(&*client, &*server).await,
    );
    eprintln!(
        "the two figures stop their clocks differently; compare each only against its own history"
    );
}

#[allow(clippy::similar_names)]
async fn tcp_socket_to_stream(client: &dyn Transport, server: &dyn Transport) -> f64 {
    let (send, recv) = client.open_bi().await.unwrap();
    let (near, far) = tokio::io::duplex(4 * 1024 * 1024);
    let pumped = tokio::spawn(pump(near, send, recv, 16 * 1024));
    let (mut far_rd, mut far_wr) = tokio::io::split(far);

    // Started before accept_bi: yamux carries the SYN on a stream's first frame, so accept_bi
    // would hang forever waiting on a stream nothing has written to.
    let writer = tokio::spawn(async move {
        let chunk = vec![0x5a_u8; 1 << 20];
        let mut sent = 0usize;
        while sent < PAYLOAD {
            far_wr.write_all(&chunk).await.unwrap();
            sent += chunk.len();
        }
        far_wr.shutdown().await.unwrap();
    });

    let (mut peer_send, mut peer_recv) = server.accept_bi().await.unwrap();
    // Only this direction is measured; the reverse would otherwise compete for the link. Finishing
    // before the reader is spawned also leaves `peer_send` unpolled: `tokio::io::split` guards the
    // two halves of a yamux stream with a blocking mutex, so one task per stream, never two.
    peer_send.finish().await;

    let started = Instant::now();
    let arrived = tokio::spawn(async move {
        let mut buf = vec![0u8; 1 << 20];
        let mut total = 0usize;
        while let Ok(n) = peer_recv.read(&mut buf).await {
            if n == 0 {
                break;
            }
            total += n;
        }
        total
    });
    let mut drain = vec![0u8; 1 << 20];
    while far_rd.read(&mut drain).await.unwrap_or(0) > 0 {}
    writer.await.unwrap();
    let secs = started.elapsed().as_secs_f64();
    let _ = pumped.await;
    assert_eq!(arrived.await.unwrap(), PAYLOAD);
    secs
}

#[allow(clippy::similar_names)]
async fn tcp_stream_to_socket(client: &dyn Transport, server: &dyn Transport) -> f64 {
    let (send, recv) = client.open_bi().await.unwrap();
    let (near, far) = tokio::io::duplex(4 * 1024 * 1024);
    let pumped = tokio::spawn(pump(near, send, recv, 16 * 1024));
    let (mut far_rd, mut far_wr) = tokio::io::split(far);

    // yamux carries the SYN on a stream's first frame, and here the peer is the one with a payload
    // to send, so the pump has to put a byte on the wire first.
    far_wr.write_all(b"x").await.unwrap();
    far_wr.shutdown().await.unwrap();

    let (mut peer_send, mut peer_recv) = server.accept_bi().await.unwrap();
    // Drained to its end before the writer is spawned, so only one task ever polls this stream.
    let mut probe = Vec::new();
    peer_recv.read_to_end(&mut probe).await.unwrap();

    let started = Instant::now();
    let writer = tokio::spawn(async move {
        let chunk = vec![0x5a_u8; 1 << 20];
        let mut sent = 0usize;
        while sent < PAYLOAD {
            peer_send.write_all(&chunk).await.unwrap();
            sent += chunk.len();
        }
        peer_send.finish().await;
    });

    let mut drain = vec![0u8; 1 << 20];
    let mut arrived = 0usize;
    loop {
        let n = far_rd.read(&mut drain).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        arrived += n;
    }
    let secs = started.elapsed().as_secs_f64();
    writer.await.unwrap();
    let _ = pumped.await;
    assert_eq!(arrived, PAYLOAD);
    secs
}
