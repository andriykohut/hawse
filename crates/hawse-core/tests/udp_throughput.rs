mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use common::{client_config_over, expect_bound, server_config, start_client, start_server};
use hawse_core::config::Prefer;
use hawse_core::identity::Identity;
use tokio::net::UdpSocket;
use tokio::time::MissedTickBehavior;

const LADDER: [u64; 5] = [20_000, 50_000, 100_000, 200_000, 400_000];
const SECONDS: u64 = 3;

/// A loss above this stops the ladder: a rung already failing would only measure how badly an
/// overloaded path fails, not the per-packet cost the baseline tracks.
const STOP_ABOVE: f64 = 0.05;

/// Counts what arrives and never answers.
async fn sink() -> (SocketAddr, Arc<AtomicU64>) {
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

/// `pps` for `SECONDS`, in one burst per millisecond.
async fn offer(to: SocketAddr, len: usize, pps: u64) -> u64 {
    let visitor = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    visitor.connect(to).await.unwrap();
    let payload = vec![0u8; len];
    let mut tick = tokio::time::interval(Duration::from_millis(1));
    tick.set_missed_tick_behavior(MissedTickBehavior::Burst);
    let mut sent = 0u64;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(SECONDS) {
        tick.tick().await;
        for _ in 0..pps / 1000 {
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

/// Offers one rung at `pps` against `seen`, prints its line, and returns the fraction of `sent`
/// that never arrived.
#[allow(clippy::cast_precision_loss)]
async fn rung(label: &str, to: SocketAddr, len: usize, pps: u64, seen: &AtomicU64) -> f64 {
    let sent = offer(to, len, pps).await;
    let delivered = seen.load(Ordering::Relaxed);
    report(&format!("{label}@{pps}"), sent, delivered);
    sent.saturating_sub(delivered) as f64 / sent as f64
}

async fn direct(len: usize) {
    for &pps in &LADDER {
        let (addr, seen) = sink().await;
        if rung(&format!("direct_{len}"), addr, len, pps, &seen).await > STOP_ABOVE {
            break;
        }
    }
}

async fn tunnelled(label: &str, prefer: Prefer, len: usize) {
    for &pps in &LADDER {
        // Fresh server, client and sink each rung, torn down after: a lingering tunnel could
        // still deliver this rung's queued packets into the next rung's count.
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
        let lost = rung(label, ([127, 0, 0, 1], port.number).into(), len, pps, &seen).await;
        client.cancel.cancel();
        server.cancel.cancel();
        if lost > STOP_ABOVE {
            break;
        }
    }
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
