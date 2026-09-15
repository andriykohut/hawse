#![allow(dead_code)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::ops::RangeInclusive;
use std::time::Duration;

use hawse_core::client::{Client, ClientError, Event};
use hawse_core::config::{ClientConfig, ClientPolicy, ClientTransport, Expose, ServerConfig};
use hawse_core::identity::Identity;
use hawse_core::server::Server;
use hawse_core::tls;
use hawse_core::transport::quic::{self, Tuning};
use hawse_proto::key::PublicKey;
use hawse_proto::port::{Port, PortSpan};
use quinn::{Connection, Endpoint};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub struct Pair {
    pub server: Connection,
    pub client: Connection,
    pub server_endpoint: Endpoint,
    pub client_endpoint: Endpoint,
}

pub async fn quic_pair() -> Pair {
    let server_id = Identity::generate().unwrap();
    let client_id = Identity::generate().unwrap();
    let (cert, key) = server_id.certificate().unwrap();
    let server_endpoint = quic::listen(
        "127.0.0.1:0".parse().unwrap(),
        tls::server_config(cert, key, tls::provider()).unwrap(),
        Tuning::SERVER,
    )
    .unwrap();
    let addr = server_endpoint.local_addr().unwrap();
    let (cert, key) = client_id.certificate().unwrap();
    let client_endpoint = quic::dialer(
        tls::client_config(cert, key, server_id.public_key(), tls::provider()).unwrap(),
        Tuning::CLIENT,
        addr,
    )
    .unwrap();
    let (server, client) = tokio::join!(
        async { server_endpoint.accept().await.unwrap().await.unwrap() },
        async { quic::connect(&client_endpoint, addr).await.unwrap() },
    );
    Pair {
        server,
        client,
        server_endpoint,
        client_endpoint,
    }
}

pub struct RunningServer {
    pub addr: SocketAddr,
    pub key: PublicKey,
    pub cancel: CancellationToken,
    pub task: JoinHandle<()>,
}

pub const DYNAMIC_PORTS: RangeInclusive<u16> = 47000..=47999;

pub fn server_config(clients: &[(&str, PublicKey, &[&str])]) -> ServerConfig {
    let mut cfg = ServerConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        dynamic_ports: PortSpan {
            first: *DYNAMIC_PORTS.start(),
            last: *DYNAMIC_PORTS.end(),
        },
        ..ServerConfig::default()
    };
    for (name, key, ports) in clients {
        let ports = ports.iter().map(|p| p.parse().unwrap()).collect();
        cfg.clients
            .insert((*name).to_owned(), ClientPolicy { key: *key, ports });
    }
    cfg
}

pub fn start_server(cfg: &ServerConfig, identity: &Identity) -> RunningServer {
    let server = Server::bind(cfg, identity).unwrap();
    let addr = server.local_addr();
    let cancel = CancellationToken::new();
    let task = tokio::spawn(server.serve(cancel.clone()));
    RunningServer {
        addr,
        key: identity.public_key(),
        cancel,
        task,
    }
}

pub fn client_config(
    server_addr: SocketAddr,
    server_key: PublicKey,
    exposes: &[(&str, &str, &str)],
) -> ClientConfig {
    let mut expose = BTreeMap::new();
    for (name, local, port) in exposes {
        expose.insert(
            (*name).to_owned(),
            Expose {
                local: (*local).to_owned(),
                port: port.parse().unwrap(),
                allow: vec![],
                proxy_protocol: false,
            },
        );
    }
    ClientConfig {
        server: server_addr.to_string(),
        server_key,
        key: "unused.key".into(),
        name: Some("test".to_owned()),
        expose,
        transport: ClientTransport::default(),
    }
}

pub struct RunningClient {
    pub events: mpsc::Receiver<Event>,
    pub cancel: CancellationToken,
    pub task: JoinHandle<Result<(), ClientError>>,
}

pub fn start_client(cfg: ClientConfig, identity: Identity) -> RunningClient {
    let (tx, events) = mpsc::channel(256);
    let cancel = CancellationToken::new();
    let client = Client::new(cfg, identity);
    let task = tokio::spawn({
        let cancel = cancel.clone();
        async move { client.run_once(cancel, &tx).await }
    });
    RunningClient {
        events,
        cancel,
        task,
    }
}

pub async fn next_event(events: &mut mpsc::Receiver<Event>) -> Event {
    tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("event within 5 s")
        .expect("event channel open")
}

pub async fn expect_bound(events: &mut mpsc::Receiver<Event>, service: &str) -> Port {
    loop {
        match next_event(events).await {
            Event::Bound { service: s, port } if s == service => return port,
            Event::BindFailed { service: s, reason } if s == service => {
                panic!("{s} failed to bind: {reason}")
            }
            _ => {}
        }
    }
}

pub async fn echo_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let (mut rd, mut wr) = socket.split();
                let _ = tokio::io::copy(&mut rd, &mut wr).await;
                let _ = wr.shutdown().await;
            });
        }
    });
    addr
}

/// A port that was free a moment ago; good enough for tests that need a fixed public port.
pub async fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A `free_port` clear of `DYNAMIC_PORTS` and of `taken`, so a fixed bind cannot collide with a
/// dynamic one. It loops because the ephemeral range overlaps the pool on Linux.
pub async fn free_port_outside_pool(taken: &[u16]) -> u16 {
    loop {
        let port = free_port().await;
        if !DYNAMIC_PORTS.contains(&port) && !taken.contains(&port) {
            return port;
        }
    }
}
