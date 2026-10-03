//! Times hawse handshakes against a running server. No `Hello` is ever sent, so the server holds
//! no session for these connections, and a key it knows is not counted by its limiter for leaving
//! early. A key it does not know is, so use an authorized one.
//!
//!     cargo run --release -p hawse-core --example handshake -- client.toml client.key 1000

use std::error::Error;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use hawse_core::config::{ClientConfig, DEFAULT_PORT, split_host_port};
use hawse_core::identity::Identity;
use hawse_core::tls;
use hawse_core::transport::quic::{self, Tuning};
use hawse_core::transport::tcp;
use hawse_proto::key::PublicKey;

type Failure = Box<dyn Error>;

struct Dial<'a> {
    identity: &'a Identity,
    server_key: PublicKey,
    tuning: Tuning,
    remote: SocketAddr,
}

impl Dial<'_> {
    fn tls(&self) -> Result<rustls::ClientConfig, Failure> {
        let (cert, key) = self.identity.certificate()?;
        Ok(tls::client_config(
            cert,
            key,
            self.server_key,
            tls::provider(),
        )?)
    }

    /// A fresh endpoint each time, as a connecting client has: a new socket and no cached state.
    async fn quic(&self) -> Result<Duration, Failure> {
        let endpoint = quic::dialer(self.tls()?, self.tuning, self.remote)?;
        let started = Instant::now();
        let conn = quic::connect(&endpoint, self.remote).await?;
        let took = started.elapsed();
        conn.close(quinn::VarInt::from_u32(0), b"");
        let _ = tokio::time::timeout(Duration::from_secs(1), endpoint.wait_idle()).await;
        Ok(took)
    }

    async fn tcp(&self) -> Result<Duration, Failure> {
        let tls = self.tls()?;
        let started = Instant::now();
        let transport = tcp::connect(self.remote, tls, self.tuning).await?;
        let took = started.elapsed();
        drop(transport);
        Ok(took)
    }
}

fn report(name: &str, mut took: Vec<Duration>, failed: usize) {
    took.sort_unstable();
    // Nearest rank: the smallest sample with at least `percent` of them at or below it.
    let at = |percent: usize| {
        let rank = (took.len() * percent).div_ceil(100).max(1);
        took.get(rank - 1)
            .map_or(f64::NAN, |d| d.as_secs_f64() * 1000.0)
    };
    println!(
        "{name} n {} failed {failed} min {:.1} p50 {:.1} p90 {:.1} p99 {:.1} max {:.1} ms",
        took.len(),
        took.first().map_or(f64::NAN, |d| d.as_secs_f64() * 1000.0),
        at(50),
        at(90),
        at(99),
        at(100),
    );
}

#[tokio::main]
async fn main() -> Result<(), Failure> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [config, key, count] = args.as_slice() else {
        return Err("usage: handshake <client.toml> <client.key> <count>".into());
    };
    let cfg: ClientConfig = toml::from_str(&std::fs::read_to_string(config)?)?;
    let identity = Identity::from_pem(&std::fs::read_to_string(key)?)?;
    let count: usize = count.parse()?;
    let (host, port) = split_host_port(&cfg.server).ok_or("server must be host or host:port")?;
    let target = format!("{host}:{}", port.unwrap_or(DEFAULT_PORT));
    let remote = tokio::net::lookup_host(&target)
        .await?
        .next()
        .ok_or("server did not resolve")?;
    let dial = Dial {
        identity: &identity,
        server_key: cfg.server_key,
        tuning: Tuning {
            idle_timeout: cfg.transport.idle_timeout,
            congestion: cfg.transport.congestion,
            stream_window: u32::try_from(cfg.transport.stream_window.0)?,
            connection_window: cfg.transport.connection_window.0,
            max_streams: Tuning::CLIENT.max_streams,
        },
        remote,
    };

    let (mut quic, mut tcp) = (Vec::with_capacity(count), Vec::with_capacity(count));
    let (mut quic_failed, mut tcp_failed) = (0, 0);
    for _ in 0..count {
        match dial.quic().await {
            Ok(took) => quic.push(took),
            Err(err) => {
                quic_failed += 1;
                eprintln!("quic: {err}");
            }
        }
        match dial.tcp().await {
            Ok(took) => tcp.push(took),
            Err(err) => {
                tcp_failed += 1;
                eprintln!("tcp: {err}");
            }
        }
    }
    report("quic", quic, quic_failed);
    report("tcp", tcp, tcp_failed);
    Ok(())
}
