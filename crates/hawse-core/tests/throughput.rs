mod common;

use std::time::Instant;

use hawse_core::pump::pump;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PAYLOAD: usize = 256 * 1024 * 1024;

#[tokio::test]
#[ignore = "benchmark, run explicitly with --ignored --nocapture"]
#[allow(clippy::similar_names, clippy::cast_precision_loss)]
async fn throughput_over_quic() {
    let pair = common::quic_pair().await;
    let (send, recv) = pair.client.open_bi().await.unwrap();
    let (near, far) = tokio::io::duplex(4 * 1024 * 1024);
    let pumped = tokio::spawn(pump(near, send, recv, 16 * 1024));
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
    tokio::spawn(async move {
        while let Ok(Some(_)) = peer_recv.read_chunk(1 << 20, true).await {}
    });

    let started = Instant::now();
    // Only this direction is measured; the reverse would otherwise compete for the link.
    peer_send.finish().unwrap();
    let mut drain = vec![0u8; 1 << 20];
    while far_rd.read(&mut drain).await.unwrap_or(0) > 0 {}
    writer.await.unwrap();
    let secs = started.elapsed().as_secs_f64();
    let _ = pumped.await;
    eprintln!(
        "throughput_over_quic {:.0} MiB/s",
        (PAYLOAD as f64 / (1024.0 * 1024.0)) / secs
    );
}
