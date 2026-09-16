mod listener;
pub mod policy;
pub mod ports;
mod session;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hawse_proto::key::PublicKey;
use quinn::{Endpoint, VarInt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::config::ServerConfig;
use crate::error::chain;
use crate::identity::{Identity, IdentityError};
use crate::transport::quic::{self, QuicError, QuicTransport, Tuning};
use crate::transport::{CloseReason, tcp};
use crate::{net, tls};
use policy::Policy;
use ports::PortAllocator;

pub const AGENT: &str = concat!("hawse/", env!("CARGO_PKG_VERSION"));

const DRAIN: Duration = Duration::from_secs(5);
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

pub struct Shared {
    pub policy: Policy,
    pub ports: Mutex<PortAllocator>,
    pub buffer: usize,
    /// One live session per client key, so a reconnecting client is not locked out of its own
    /// ports by the session its previous connection left behind.
    pub sessions: Mutex<HashMap<PublicKey, Arc<Live>>>,
}

/// `done` is cancelled once the session has released its ports, so a session superseding this one
/// can wait for them.
pub struct Live {
    pub cancel: CancellationToken,
    pub done: CancellationToken,
}

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error("TLS setup failed")]
    Tls(#[from] rustls::Error),
    #[error(transparent)]
    Quic(#[from] QuicError),
    #[error("cannot bind the TCP listener on the listen port")]
    Listen(#[source] std::io::Error),
    #[error("stream window {0} bytes does not fit a QUIC window")]
    Window(u64),
}

pub struct Server {
    endpoint: Endpoint,
    tcp: TcpListener,
    tls: Arc<rustls::ServerConfig>,
    tuning: Tuning,
    shared: Arc<Shared>,
}

impl Server {
    pub fn bind(cfg: &ServerConfig, identity: &Identity) -> Result<Self, ServerError> {
        let stream_window = u32::try_from(cfg.transport.stream_window.0)
            .map_err(|_| ServerError::Window(cfg.transport.stream_window.0))?;
        let tuning = Tuning {
            idle_timeout: cfg.transport.idle_timeout,
            congestion: cfg.transport.congestion,
            stream_window,
            connection_window: cfg.transport.connection_window.0,
            max_streams: cfg.limits.streams_per_client,
        };
        // Cloned rather than minted twice, so both listeners present the same certificate and not
        // just the same key.
        let (cert, key) = identity.certificate()?;
        let endpoint = quic::listen(
            cfg.listen,
            tls::server_config(cert.clone(), key.clone_key(), tls::provider())?,
            tuning,
        )?;
        // Both listeners must answer on one address, and only the bound endpoint knows which:
        // `listen` may have fallen back to IPv4, and a port of 0 is decided by the kernel.
        let bound = endpoint
            .local_addr()
            .expect("a bound endpoint has an address");
        let tcp = net::bind_tcp(bound.ip(), bound.port()).map_err(ServerError::Listen)?;
        let tls = Arc::new(tls::server_config(cert, key, tls::provider())?);
        let mut ports = PortAllocator::new(cfg.dynamic_ports);
        ports.reserve(bound.port());
        let shared = Arc::new(Shared {
            policy: Policy::from_config(cfg),
            ports: Mutex::new(ports),
            buffer: usize::try_from(cfg.transport.buffer.0).expect("a validated buffer"),
            sessions: Mutex::new(HashMap::new()),
        });
        Ok(Self {
            endpoint,
            tcp,
            tls,
            tuning,
            shared,
        })
    }

    /// The UDP port QUIC answers on; the TCP fallback shares its number.
    pub fn local_addr(&self) -> SocketAddr {
        self.endpoint
            .local_addr()
            .expect("a bound endpoint has an address")
    }

    /// Runs until `cancel` fires or the QUIC endpoint stops accepting. Cancelling reaches every
    /// session through a child token, so clients receive `Shutdown`; the endpoint then closes,
    /// waiting up to 5 s for sessions to drain first.
    pub async fn serve(self, cancel: CancellationToken) {
        let sessions = TaskTracker::new();
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                incoming = self.endpoint.accept() => {
                    let Some(incoming) = incoming else { break };
                    let shared = Arc::clone(&self.shared);
                    let cancel = cancel.child_token();
                    sessions.spawn(async move {
                        match incoming.await {
                            Ok(conn) => session::run(Arc::new(QuicTransport(conn)), shared, cancel).await,
                            Err(err) => tracing::debug!(err = %chain(&err), "handshake failed"),
                        }
                    });
                }
                accepted = self.tcp.accept() => {
                    let socket = match accepted {
                        Ok((socket, _)) => socket,
                        Err(err) => {
                            tracing::warn!(err = %chain(&err), "tcp accept failed");
                            // A descriptor shortage fails every accept at once, and without this
                            // the loop spins on it.
                            tokio::time::sleep(ACCEPT_BACKOFF).await;
                            continue;
                        }
                    };
                    let shared = Arc::clone(&self.shared);
                    let cancel = cancel.child_token();
                    let tls = Arc::clone(&self.tls);
                    let tuning = self.tuning;
                    sessions.spawn(async move {
                        match tcp::accept(socket, tls, tuning).await {
                            Ok(transport) => session::run(Arc::new(transport), shared, cancel).await,
                            Err(err) => tracing::debug!(err = %chain(&err), "tcp handshake failed"),
                        }
                    });
                }
            }
        }
        sessions.close();
        if tokio::time::timeout(DRAIN, sessions.wait()).await.is_err() {
            tracing::warn!("some sessions did not drain in time");
        }
        let shutdown = CloseReason::Shutdown;
        self.endpoint.close(
            VarInt::from_u32(shutdown.code()),
            shutdown.as_str().as_bytes(),
        );
        self.endpoint.wait_idle().await;
    }
}
