mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use common::{client_config_over, expect_bound, server_config, start_client, start_server};
use hawse_core::config::Prefer;
use hawse_core::identity::Identity;
use tokio::net::UdpSocket;
use tokio::time::MissedTickBehavior;

const OFFERED_PPS: u64 = 20_000;
const SECONDS: u64 = 5;

/// Counts what arrives and never answers.
async fn sink() -> (std::net::SocketAddr, Arc<AtomicU64>) {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    let seen = Arc::new(AtomicU64::new(0));
    tokio::spawn({
        let seen = Arc::clone(&seen);
        async move {
            let mut buf = vec![0u8; 65536];
            while socket.recv(&mut buf).await.is_ok() {
                seen.fetch_add(1, Ordering::Relaxed);
            }
        }
    });
    (addr, seen)
}

/// `OFFERED_PPS` for `SECONDS`, in one burst per millisecond.
async fn offer(to: std::net::SocketAddr, len: usize) -> u64 {
    let visitor = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    visitor.connect(to).await.unwrap();
    let payload = vec![0u8; len];
    let mut tick = tokio::time::interval(Duration::from_millis(1));
    tick.set_missed_tick_behavior(MissedTickBehavior::Burst);
    let mut sent = 0u64;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(SECONDS) {
        tick.tick().await;
        for _ in 0..OFFERED_PPS / 1000 {
            if visitor.send(&payload).await.is_ok() {
                sent += 1;
            }
        }
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    sent
}

#[allow(clippy::cast_precision_loss)]
fn report(label: &str, sent: u64, seen: u64) {
    eprintln!(
        "udp_{label} offered {} pps, delivered {} pps, lost {:.2}%",
        sent / SECONDS,
        seen / SECONDS,
        100.0 * (sent.saturating_sub(seen)) as f64 / sent as f64
    );
}

async fn direct(len: usize) {
    let (addr, seen) = sink().await;
    let sent = offer(addr, len).await;
    report(&format!("direct_{len}"), sent, seen.load(Ordering::Relaxed));
}

async fn tunnelled(label: &str, prefer: Prefer, len: usize) {
    let server_id = Identity::generate().unwrap();
    let client_id = Identity::generate().unwrap();
    let server = start_server(
        &server_config(&[("test", client_id.public_key(), &[])]),
        &server_id,
    );
    let (addr, seen) = sink().await;
    let mut client = start_client(
        client_config_over(
            server.addr,
            server.key,
            &[("sink", &addr.to_string(), "any/udp")],
            prefer,
        ),
        client_id,
    );
    let port = expect_bound(&mut client.events, "sink").await;
    let sent = offer(([127, 0, 0, 1], port.number).into(), len).await;
    report(label, sent, seen.load(Ordering::Relaxed));
    client.cancel.cancel();
    server.cancel.cancel();
}

// One test, run in sequence, for the reason `throughput.rs` gives: parallel test functions would
// race over the same loopback and measure contention. `direct` is the ceiling of the harness
// itself: loss there is the test's own sockets, not the tunnel.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark, run explicitly with --release --ignored --nocapture"]
async fn udp_packet_rate_over_quic_and_the_tcp_fallback() {
    direct(1200).await;
    tunnelled("quic_datagram_1200", Prefer::Quic, 1200).await;
    tunnelled("quic_bulk_1472", Prefer::Quic, 1472).await;
    tunnelled("tcp_bulk_1200", Prefer::Tcp, 1200).await;
    eprintln!("loopback: no loss, no round trip. Compare each line only against its own history.");
}
