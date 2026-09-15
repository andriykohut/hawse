#![allow(dead_code)]

use hawse_core::identity::Identity;
use hawse_core::tls;
use hawse_core::transport::quic::{self, Tuning};
use quinn::{Connection, Endpoint};

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
