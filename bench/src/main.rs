//! Measures what a tunnel costs: bulk throughput, and whether one bulk transfer
//! starves a small concurrent request. Point it at a forwarded port or at the
//! service directly; it does not know or care which.

use std::env;
use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const PROBE: &[u8] = b"probe...";

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let usage =
        "usage: hawse-bench sink <port> | hawse-bench load <host:port> <seconds> <probes> [mib_s]";
    match args.get(1).map(String::as_str) {
        Some("sink") => {
            let Some(port) = args.get(2).and_then(|p| p.parse::<u16>().ok()) else {
                eprintln!("{usage}");
                return ExitCode::from(2);
            };
            sink(port).await;
            ExitCode::SUCCESS
        }
        Some("load") => {
            let (Some(target), Some(seconds), Some(probes)) = (
                args.get(2).and_then(|t| t.parse::<SocketAddr>().ok()),
                args.get(3).and_then(|s| s.parse::<u64>().ok()),
                args.get(4).and_then(|p| p.parse::<usize>().ok()),
            ) else {
                eprintln!("{usage}");
                return ExitCode::from(2);
            };
            let rate = args.get(5).and_then(|r| r.parse::<f64>().ok());
            load(target, Duration::from_secs(seconds), probes, rate).await;
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("{usage}");
            ExitCode::from(2)
        }
    }
}

/// Drains whatever a bulk connection sends, and echoes a probe connection back.
/// The two are told apart by their first byte: bulk sends `b'B'`, probe `b'P'`.
async fn sink(port: u16) {
    let listener = TcpListener::bind(("127.0.0.1", port)).await.expect("bind");
    println!("{}", listener.local_addr().expect("addr"));
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            continue;
        };
        tokio::spawn(async move {
            let mut kind = [0u8; 1];
            if socket.read_exact(&mut kind).await.is_err() {
                return;
            }
            let _ = socket.set_nodelay(true);
            if kind[0] == b'B' {
                let mut buf = vec![0u8; 256 * 1024];
                while let Ok(n) = socket.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                }
            } else {
                let mut buf = [0u8; PROBE.len()];
                while socket.read_exact(&mut buf).await.is_ok() {
                    if socket.write_all(&buf).await.is_err() {
                        break;
                    }
                }
            }
        });
    }
}

async fn load(target: SocketAddr, duration: Duration, probes: usize, rate: Option<f64>) {
    let unloaded = probe_latencies(target, probes, Duration::ZERO).await;

    let bulk = tokio::spawn(async move { push(target, duration, rate).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let loaded = probe_latencies(target, probes, Duration::from_millis(20)).await;
    let bytes = bulk.await.expect("bulk task");

    let secs = duration.as_secs_f64();
    let mib = bytes as f64 / (1024.0 * 1024.0);
    println!("throughput_mib_s {:.1}", mib / secs);
    report("probe_unloaded", &unloaded);
    report("probe_loaded", &loaded);
}

/// Writes for `duration` and returns the byte count the peer accepted. A rate in
/// MiB/s paces the writes, which is what makes the probe measure the tunnel
/// rather than a saturated CPU.
async fn push(target: SocketAddr, duration: Duration, rate: Option<f64>) -> u64 {
    let mut socket = TcpStream::connect(target).await.expect("bulk connect");
    let _ = socket.set_nodelay(true);
    socket.write_all(b"B").await.expect("bulk kind");
    let chunk_len = if rate.is_some() { 32 * 1024 } else { 256 * 1024 };
    let chunk = vec![0x5a_u8; chunk_len];
    let started = Instant::now();
    let deadline = started + duration;
    let bytes_per_sec = rate.map(|mib| mib * 1024.0 * 1024.0);
    let mut total = 0u64;
    while Instant::now() < deadline {
        match socket.write_all(&chunk).await {
            Ok(()) => total += chunk.len() as u64,
            Err(_) => break,
        }
        if let Some(per_sec) = bytes_per_sec {
            let owed = Duration::from_secs_f64(total as f64 / per_sec);
            let spent = started.elapsed();
            if owed > spent {
                tokio::time::sleep(owed - spent).await;
            }
        }
    }
    total
}

/// Round-trips a small payload on its own connection, one at a time.
async fn probe_latencies(target: SocketAddr, count: usize, gap: Duration) -> Vec<Duration> {
    let mut socket = match TcpStream::connect(target).await {
        Ok(socket) => socket,
        Err(_) => return Vec::new(),
    };
    let _ = socket.set_nodelay(true);
    if socket.write_all(b"P").await.is_err() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(count);
    let mut buf = [0u8; PROBE.len()];
    for _ in 0..count {
        let started = Instant::now();
        if socket.write_all(PROBE).await.is_err() || socket.read_exact(&mut buf).await.is_err() {
            break;
        }
        out.push(started.elapsed());
        if !gap.is_zero() {
            tokio::time::sleep(gap).await;
        }
    }
    out
}

fn report(label: &str, samples: &[Duration]) {
    if samples.is_empty() {
        println!("{label} none");
        return;
    }
    let mut ms: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    ms.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    let at = |q: f64| ms[((ms.len() - 1) as f64 * q).round() as usize];
    println!(
        "{label} p50 {:.2} p99 {:.2} max {:.2} n {}",
        at(0.50),
        at(0.99),
        ms[ms.len() - 1],
        ms.len()
    );
}
