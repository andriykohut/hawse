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
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::config::ServerConfig;
use crate::error::chain;
use crate::identity::{Identity, IdentityError};
use crate::tls;
use crate::transport::quic::{self, QuicError, Tuning};
use policy::Policy;
use ports::PortAllocator;

pub const AGENT: &str = concat!("hawse/", env!("CARGO_PKG_VERSION"));

const DRAIN: Duration = Duration::from_secs(5);

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
    #[error("stream window {0} bytes does not fit a QUIC window")]
    Window(u64),
}

pub struct Server {
    endpoint: Endpoint,
    shared: Arc<Shared>,
}

impl Server {
    pub fn bind(cfg: &ServerConfig, identity: &Identity) -> Result<Self, ServerError> {
        let (cert, key) = identity.certificate()?;
        let tls = tls::server_config(cert, key, tls::provider())?;
        let stream_window = u32::try_from(cfg.transport.stream_window.0)
            .map_err(|_| ServerError::Window(cfg.transport.stream_window.0))?;
        let tuning = Tuning {
            idle_timeout: cfg.transport.idle_timeout,
            stream_window,
            connection_window: cfg.transport.connection_window.0,
            max_streams: cfg.limits.streams_per_client,
        };
        let endpoint = quic::listen(cfg.listen, tls, tuning)?;
        let shared = Arc::new(Shared {
            policy: Policy::from_config(cfg),
            ports: Mutex::new(PortAllocator::new(cfg.dynamic_ports)),
            buffer: usize::try_from(cfg.transport.buffer.0).expect("a validated buffer"),
            sessions: Mutex::new(HashMap::new()),
        });
        Ok(Self { endpoint, shared })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.endpoint
            .local_addr()
            .expect("a bound endpoint has an address")
    }

    /// Runs until `cancel` fires or the endpoint stops accepting. Cancelling reaches every session
    /// through a child token, so clients receive `Shutdown`; the endpoint then closes, waiting up
    /// to 5 s for sessions to drain first.
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
                            Ok(conn) => session::run(conn, shared, cancel).await,
                            Err(err) => tracing::debug!(err = %chain(&err), "handshake failed"),
                        }
                    });
                }
            }
        }
        sessions.close();
        if tokio::time::timeout(DRAIN, sessions.wait()).await.is_err() {
            tracing::warn!("some sessions did not drain in time");
        }
        self.endpoint.close(VarInt::from_u32(0), b"shutdown");
        self.endpoint.wait_idle().await;
    }
}
