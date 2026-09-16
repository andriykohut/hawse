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

/// The only accept failure worth pausing for. `accept` also reports a connection the peer reset
/// between the SYN and the accept — `ECONNABORTED`, `EPROTO` — and pausing on those would let a
/// peer that connects and resets in a loop pace this server's whole accept loop for free.
///
/// `io::ErrorKind` names neither of these, and their numbers are identical on every platform hawse
/// builds for.
fn out_of_descriptors(err: &std::io::Error) -> bool {
    matches!(err.raw_os_error(), Some(ENFILE | EMFILE))
}

const ENFILE: i32 = 23;
const EMFILE: i32 = 24;

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
    #[error(
        "cannot bind TCP {0}: the listen port now carries the TCP fallback transport as well as QUIC, so it must be free on TCP too"
    )]
    Listen(SocketAddr, #[source] std::io::Error),
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
        let tcp = net::bind_tcp(bound.ip(), bound.port())
            .map_err(|err| ServerError::Listen(bound, err))?;
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
        let tcp = &self.tcp;
        // An instant, not a duration: the arm holding it is rebuilt every time another arm wins,
        // and a relative sleep would restart from zero each time and never elapse.
        let mut resume_tcp: Option<tokio::time::Instant> = None;
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
                // The pause waits inside this arm rather than in its body, so a descriptor
                // shortage on the TCP side cannot stall QUIC, which needs no descriptor of its own.
                // It cannot move to a `, if …` precondition on the arm either: a guarded arm
                // builds no future, so nothing holds the timer, and with QUIC idle the arm would
                // never wake to re-enable itself.
                accepted = async move {
                    if let Some(at) = resume_tcp {
                        tokio::time::sleep_until(at).await;
                    }
                    tcp.accept().await
                } => {
                    let socket = match accepted {
                        Ok((socket, _)) => {
                            resume_tcp = None;
                            socket
                        }
                        Err(err) if out_of_descriptors(&err) => {
                            tracing::warn!(err = %chain(&err), "tcp accept failed");
                            // Every accept fails until something closes, so without this the arm
                            // spins on it.
                            resume_tcp = Some(tokio::time::Instant::now() + ACCEPT_BACKOFF);
                            continue;
                        }
                        Err(err) => {
                            tracing::debug!(err = %chain(&err), "tcp accept failed");
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn a_descriptor_shortage_is_told_apart_from_a_reset_connection() {
        assert!(out_of_descriptors(&io::Error::from_raw_os_error(ENFILE)));
        assert!(out_of_descriptors(&io::Error::from_raw_os_error(EMFILE)));
        // Linux's ECONNABORTED and EPROTO — what a peer resetting between the SYN and the accept
        // reaches this code as. They have to be errno values rather than `ErrorKind`s: a negative
        // case carrying no errno at all leaves `Some(_)` passing for the allowlist.
        for errno in [103, 71] {
            assert!(!out_of_descriptors(&io::Error::from_raw_os_error(errno)));
        }
        assert!(!out_of_descriptors(&io::Error::other("not an os error")));
    }
}
