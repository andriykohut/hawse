use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::future::BoxFuture;
use hawse_proto::key::PublicKey;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{
    ClientConfig, Connection, Endpoint, IdleTimeout, ServerConfig, TransportConfig, VarInt,
};
use rustls::pki_types::CertificateDer;

use crate::config::Congestion;
use crate::tls;
use crate::transport::{CloseReason, RecvHalf, SendHalf, Transport, TransportError};

/// Both transports take one of these, though it lives here. `stream_window` and `congestion` are
/// QUIC's alone — yamux fixes every stream's window at `DEFAULT_CREDIT` and TCP's congestion
/// control is the kernel's — so under `Prefer::Tcp` the two are accepted, validated and inert.
/// `idle_timeout` becomes the TCP handshake deadline, the keepalive idle time and the close grace;
/// `connection_window` and `max_streams` are honoured by both.
#[derive(Clone, Copy, Debug)]
pub struct Tuning {
    pub idle_timeout: Duration,
    pub congestion: Congestion,
    pub stream_window: u32,
    pub connection_window: u64,
    pub max_streams: u32,
}

impl Tuning {
    pub const SERVER: Self = Self {
        idle_timeout: Duration::from_secs(30),
        congestion: Congestion::Cubic,
        stream_window: 8 * 1024 * 1024,
        connection_window: 64 * 1024 * 1024,
        max_streams: 4096,
    };

    pub const CLIENT: Self = Self {
        idle_timeout: Duration::from_secs(30),
        congestion: Congestion::Cubic,
        stream_window: 2 * 1024 * 1024,
        connection_window: 16 * 1024 * 1024,
        max_streams: 4096,
    };
}

#[derive(Debug, thiserror::Error)]
pub enum QuicError {
    #[error("TLS configuration has no cipher suite usable for QUIC")]
    Crypto(#[from] quinn::crypto::rustls::NoInitialCipherSuite),
    #[error("transport parameter is out of range")]
    Param(#[from] quinn::VarIntBoundsExceeded),
    #[error("cannot bind UDP socket")]
    Bind(#[source] std::io::Error),
    #[error("connection attempt could not start")]
    Connect(#[from] quinn::ConnectError),
    #[error("connection failed")]
    Connection(#[from] quinn::ConnectionError),
}

fn transport_config(t: Tuning) -> Result<TransportConfig, QuicError> {
    let mut tc = TransportConfig::default();
    tc.max_idle_timeout(Some(IdleTimeout::try_from(t.idle_timeout)?));
    tc.keep_alive_interval(None);
    // Bounds what the *peer* may open toward us, so the client's value caps concurrent visitors.
    tc.max_concurrent_bidi_streams(VarInt::from_u32(t.max_streams));
    tc.max_concurrent_uni_streams(VarInt::from_u32(0));
    tc.stream_receive_window(VarInt::from_u32(t.stream_window));
    tc.receive_window(VarInt::from_u64(t.connection_window)?);
    match t.congestion {
        Congestion::Cubic => {
            tc.congestion_controller_factory(Arc::new(quinn::congestion::CubicConfig::default()))
        }
        Congestion::Bbr => {
            tc.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()))
        }
    };
    tc.datagram_receive_buffer_size(Some(2 * 1024 * 1024));
    tc.datagram_send_buffer_size(1024 * 1024);
    Ok(tc)
}

/// An unspecified IPv6 address the host cannot bind falls back to `0.0.0.0` on the same port.
pub fn listen(
    addr: SocketAddr,
    tls: rustls::ServerConfig,
    tuning: Tuning,
) -> Result<Endpoint, QuicError> {
    let crypto = QuicServerConfig::try_from(tls)?;
    let mut cfg = ServerConfig::with_crypto(Arc::new(crypto));
    cfg.transport_config(Arc::new(transport_config(tuning)?));
    match Endpoint::server(cfg.clone(), addr) {
        Ok(endpoint) => Ok(endpoint),
        Err(err) if addr.is_ipv6() && addr.ip().is_unspecified() => {
            let v4 = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), addr.port());
            Endpoint::server(cfg, v4).map_err(|_| QuicError::Bind(err))
        }
        Err(err) => Err(QuicError::Bind(err)),
    }
}

pub fn dialer(
    tls: rustls::ClientConfig,
    tuning: Tuning,
    remote: SocketAddr,
) -> Result<Endpoint, QuicError> {
    let bind: SocketAddr = if remote.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    }
    .parse()
    .expect("literal socket address");
    let mut endpoint = Endpoint::client(bind).map_err(QuicError::Bind)?;
    let crypto = QuicClientConfig::try_from(tls)?;
    let mut cfg = ClientConfig::new(Arc::new(crypto));
    cfg.transport_config(Arc::new(transport_config(tuning)?));
    endpoint.set_default_client_config(cfg);
    Ok(endpoint)
}

pub async fn connect(endpoint: &Endpoint, remote: SocketAddr) -> Result<Connection, QuicError> {
    // The pinned-key verifier ignores server names.
    Ok(endpoint.connect(remote, "hawse")?.await?)
}

#[derive(Debug)]
pub struct QuicTransport(pub Connection);

impl Transport for QuicTransport {
    fn open_bi(&self) -> BoxFuture<'_, Result<(SendHalf, RecvHalf), TransportError>> {
        Box::pin(async move {
            let (send, recv) = self
                .0
                .open_bi()
                .await
                .map_err(|e| TransportError::Connection(Box::new(e)))?;
            Ok((SendHalf::Quic(send), RecvHalf::Quic(recv)))
        })
    }

    fn accept_bi(&self) -> BoxFuture<'_, Result<(SendHalf, RecvHalf), TransportError>> {
        Box::pin(async move {
            let (send, recv) = self
                .0
                .accept_bi()
                .await
                .map_err(|e| TransportError::Connection(Box::new(e)))?;
            Ok((SendHalf::Quic(send), RecvHalf::Quic(recv)))
        })
    }

    fn send_datagram(&self, data: Bytes) -> Result<(), TransportError> {
        self.0
            .send_datagram(data)
            .map_err(|e| TransportError::Connection(Box::new(e)))
    }

    fn recv_datagram(&self) -> BoxFuture<'_, Result<Bytes, TransportError>> {
        Box::pin(async move {
            self.0
                .read_datagram()
                .await
                .map_err(|e| TransportError::Connection(Box::new(e)))
        })
    }

    fn max_datagram_size(&self) -> Option<usize> {
        self.0.max_datagram_size()
    }

    fn close(&self, reason: CloseReason) {
        self.0
            .close(VarInt::from_u32(reason.code()), reason.as_str().as_bytes());
    }

    fn closed(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.0.closed().await;
        })
    }

    fn remote_address(&self) -> SocketAddr {
        self.0.remote_address()
    }

    fn peer_key(&self) -> Option<PublicKey> {
        peer_key(&self.0)
    }
}

pub fn peer_key(conn: &Connection) -> Option<PublicKey> {
    let certs = conn
        .peer_identity()?
        .downcast::<Vec<CertificateDer<'static>>>()
        .ok()?;
    tls::peer_key(certs.first()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;
    use crate::tls;
    use hawse_proto::msg::ALPN;
    use tokio::time::timeout;

    fn server_endpoint(id: &Identity) -> Endpoint {
        let (cert, key) = id.certificate().unwrap();
        let cfg = tls::server_config(cert, key, tls::provider()).unwrap();
        listen("127.0.0.1:0".parse().unwrap(), cfg, Tuning::SERVER).unwrap()
    }

    fn client_endpoint(id: &Identity, pinned: PublicKey, remote: SocketAddr) -> Endpoint {
        let (cert, key) = id.certificate().unwrap();
        let cfg = tls::client_config(cert, key, pinned, tls::provider()).unwrap();
        dialer(cfg, Tuning::CLIENT, remote).unwrap()
    }

    #[tokio::test]
    async fn handshake_exposes_peer_keys_and_alpn() {
        let server_id = Identity::generate().unwrap();
        let client_id = Identity::generate().unwrap();
        let server = server_endpoint(&server_id);
        let addr = server.local_addr().unwrap();
        let client = client_endpoint(&client_id, server_id.public_key(), addr);

        let (server_conn, client_conn) = tokio::join!(
            async { server.accept().await.unwrap().await.unwrap() },
            async { connect(&client, addr).await.unwrap() },
        );

        assert_eq!(peer_key(&server_conn), Some(client_id.public_key()));
        assert_eq!(peer_key(&client_conn), Some(server_id.public_key()));
        let hd = client_conn
            .handshake_data()
            .unwrap()
            .downcast::<quinn::crypto::rustls::HandshakeData>()
            .unwrap();
        assert_eq!(hd.protocol.as_deref(), Some(ALPN));

        let (mut send, _recv) = client_conn.open_bi().await.unwrap();
        send.write_all(b"hi").await.unwrap();
        send.finish().unwrap();
        let (_send, mut recv) = server_conn.accept_bi().await.unwrap();
        assert_eq!(recv.read_to_end(16).await.unwrap(), b"hi");
    }

    #[tokio::test]
    async fn wrong_pinned_key_fails_the_connection() {
        let server_id = Identity::generate().unwrap();
        let client_id = Identity::generate().unwrap();
        let impostor = Identity::generate().unwrap();
        let server = server_endpoint(&server_id);
        let addr = server.local_addr().unwrap();
        let client = client_endpoint(&client_id, impostor.public_key(), addr);

        let accept = tokio::spawn(async move {
            let incoming = server.accept().await.unwrap();
            incoming.await.is_err()
        });
        let err = connect(&client, addr).await.unwrap_err();
        assert!(matches!(err, QuicError::Connection(_)), "{err:?}");
        assert!(accept.await.unwrap());
    }

    #[tokio::test]
    async fn unspecified_v6_listen_falls_back_to_v4_when_needed() {
        let id = Identity::generate().unwrap();
        let (cert, key) = id.certificate().unwrap();
        let cfg = tls::server_config(cert, key, tls::provider()).unwrap();
        let endpoint = listen("[::]:0".parse().unwrap(), cfg, Tuning::SERVER).unwrap();
        assert!(endpoint.local_addr().unwrap().ip().is_unspecified());
    }

    #[tokio::test]
    async fn client_grants_credit_for_many_concurrent_streams() {
        const BURST: usize = 512;

        let server_id = Identity::generate().unwrap();
        let client_id = Identity::generate().unwrap();
        let server = server_endpoint(&server_id);
        let addr = server.local_addr().unwrap();
        let client = client_endpoint(&client_id, server_id.public_key(), addr);

        let (server_conn, client_conn) = tokio::join!(
            async { server.accept().await.unwrap().await.unwrap() },
            async { connect(&client, addr).await.unwrap() },
        );

        let accepting = tokio::spawn(async move {
            let mut open = Vec::with_capacity(BURST);
            while open.len() < BURST {
                open.push(client_conn.accept_bi().await.unwrap());
            }
            open
        });

        let opening = async {
            let mut open = Vec::with_capacity(BURST);
            for _ in 0..BURST {
                let (mut send, recv) = server_conn.open_bi().await.unwrap();
                send.write_all(b"x").await.unwrap();
                open.push((send, recv));
            }
            open
        };

        let opened = timeout(Duration::from_secs(10), opening)
            .await
            .expect("open_bi stalled: the client granted fewer than BURST concurrent bidi streams");
        assert_eq!(opened.len(), BURST);
        assert_eq!(accepting.await.unwrap().len(), BURST);
    }
}
