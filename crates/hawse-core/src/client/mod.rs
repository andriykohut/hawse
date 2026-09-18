mod udp;
mod visitor;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use hawse_proto::key::PublicKey;
use hawse_proto::msg::{BindFailure, ClientMessage, ServerMessage, StreamOpen};
use hawse_proto::port::{Kind, Port, PortRequest};
use quinn::Endpoint;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::config::{ClientConfig, DEFAULT_PORT, Prefer, split_host_port};
use crate::control::Control;
use crate::error::chain;
use crate::frame::{StreamFrameError, read_frame};
use crate::identity::{Identity, IdentityError};
use crate::tls;
use crate::transport::quic::{self, QuicError, QuicTransport, Tuning};
use crate::transport::{
    CloseReason, RecvHalf, SendHalf, Transport, TransportError, TransportKind, tcp,
};
use crate::udp::IDLE;

pub const AGENT: &str = concat!("hawse/", env!("CARGO_PKG_VERSION"));

const PING_EVERY: Duration = Duration::from_secs(15);
const PONG_DEADLINE: Duration = Duration::from_secs(45);
const RETRY_AFTER: Duration = Duration::from_secs(5);
const DRAIN: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Connected {
        remote: SocketAddr,
        transport: TransportKind,
        name: String,
        agent: String,
    },
    Bound {
        service: String,
        port: Port,
    },
    BindFailed {
        service: String,
        reason: BindFailure,
    },
    Denied {
        key: PublicKey,
    },
    Disconnected {
        cause: DisconnectCause,
    },
}

/// `Config` came from an error retrying cannot fix; every other variant is worth retrying.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DisconnectCause {
    Shutdown(String),
    Unresponsive,
    Denied,
    Transport(String),
    Config(String),
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("server `{0}` must be host or host:port")]
    ServerAddr(String),
    #[error("cannot resolve {0}")]
    Resolve(String, #[source] std::io::Error),
    #[error("{0} did not resolve to any address")]
    NoAddress(String),
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error("TLS setup failed")]
    Tls(#[from] rustls::Error),
    #[error(transparent)]
    Quic(#[from] QuicError),
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("this machine's key {0} is not authorized on the server")]
    Denied(PublicKey),
    #[error("server sent {0} where a greeting was expected")]
    Protocol(&'static str),
    #[error("control stream closed")]
    ControlClosed,
    #[error("cannot encode a control message")]
    Encode(#[source] hawse_proto::frame::FrameError),
    #[error("server ended the session: {0}")]
    Shutdown(String),
    #[error("server stopped answering")]
    Unresponsive,
    #[error("stream window does not fit a QUIC window")]
    Window,
}

#[derive(Clone, Debug)]
pub struct Target {
    pub service: String,
    pub local: String,
}

type Targets = Arc<RwLock<HashMap<u16, Target>>>;

/// Carries the QUIC endpoint only so `wait_idle` can flush the close frame; the TCP transport has
/// none.
struct Dialed {
    transport: Arc<dyn Transport>,
    kind: TransportKind,
    endpoint: Option<Endpoint>,
    control: Control,
}

/// Bundled to keep `register_bound` under clippy's argument-count limit, and threaded through
/// to `UdpLocal::new` for the same reason.
struct BindCtx<'a> {
    transport: &'a Arc<dyn Transport>,
    tasks: &'a TaskTracker,
    udp_cancel: &'a CancellationToken,
}

async fn open_control(transport: &Arc<dyn Transport>) -> Result<Control, ClientError> {
    let (send, recv) = transport.open_bi().await?;
    Ok(Control::new(send, recv))
}

fn ping_interval() -> tokio::time::Interval {
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + PING_EVERY, PING_EVERY);
    ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    ping
}

async fn serve_stream(
    send: SendHalf,
    mut recv: RecvHalf,
    targets: Targets,
    udp: Arc<udp::Registry>,
    buffer: usize,
) {
    match read_frame::<StreamOpen>(&mut recv).await {
        Ok(StreamOpen::Visitor(header)) => {
            visitor::serve(header, send, recv, targets, buffer).await;
        }
        Ok(StreamOpen::Bulk { service_id }) => udp::serve_bulk(service_id, send, recv, udp).await,
        Err(err) => tracing::debug!(err = %chain(&err), "bad stream header"),
    }
}

pub struct Client {
    cfg: ClientConfig,
    identity: Identity,
    targets: Targets,
    udp: Arc<udp::Registry>,
}

impl Client {
    pub fn new(cfg: ClientConfig, identity: Identity) -> Self {
        Self {
            cfg,
            identity,
            targets: Arc::default(),
            udp: Arc::default(),
        }
    }

    /// Reconnects 5 s after every failure until `cancel` fires, except a `Config` cause,
    /// which retrying cannot fix and ends it instead.
    pub async fn run(&self, cancel: CancellationToken, events: mpsc::Sender<Event>) {
        while !cancel.is_cancelled() {
            match self.run_once(cancel.clone(), &events).await {
                Ok(()) => return,
                Err(err) => {
                    let cause = classify(&err);
                    let fatal = matches!(cause, DisconnectCause::Config(_));
                    emit(&events, Event::Disconnected { cause });
                    if fatal {
                        return;
                    }
                }
            }
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(RETRY_AFTER) => {}
            }
        }
    }

    /// One session. `Ok` means `cancel` ended it; every other ending is an error.
    pub async fn run_once(
        &self,
        cancel: CancellationToken,
        events: &mpsc::Sender<Event>,
    ) -> Result<(), ClientError> {
        let remote = self.resolve().await?;
        let Dialed {
            transport,
            kind,
            endpoint,
            mut control,
        } = self.connect(remote).await?;

        let (agent, client_name) = self.greet(&mut control, &transport, events).await?;
        emit(
            events,
            Event::Connected {
                remote,
                transport: kind,
                name: client_name,
                agent,
            },
        );

        self.send_binds(&mut control).await?;

        self.targets.write().expect("targets lock").clear();
        let tasks = TaskTracker::new();
        self.udp.clear();
        let udp_cancel = CancellationToken::new();
        let bind_ctx = BindCtx {
            transport: &transport,
            tasks: &tasks,
            udp_cancel: &udp_cancel,
        };
        tasks.spawn(udp::demux(Arc::clone(&transport), Arc::clone(&self.udp)));
        let buffer = usize::try_from(self.cfg.transport.buffer.0).expect("a validated buffer");
        let mut ping = ping_interval();
        let mut last_heard = Instant::now();
        let mut nonce = 0u64;

        let outcome = loop {
            tokio::select! {
                () = cancel.cancelled() => break Ok(()),
                _ = ping.tick() => {
                    if last_heard.elapsed() > PONG_DEADLINE {
                        break Err(ClientError::Unresponsive);
                    }
                    nonce += 1;
                    if let Err(err) = send_msg(&mut control, &ClientMessage::Ping { nonce }).await {
                        break Err(err);
                    }
                }
                msg = control.next::<ServerMessage>() => {
                    let Some(msg) = msg else { break Err(ClientError::ControlClosed) };
                    last_heard = Instant::now();
                    match msg {
                        ServerMessage::Bound { service, service_id, port } => {
                            let Some(expose) = self.cfg.expose.get(&service) else {
                                tracing::warn!(service, "server bound a service we never asked for");
                                continue;
                            };
                            let kind = expose.port.kind();
                            self.register_bound(service.clone(), service_id, expose.local.clone(), kind, &bind_ctx);
                            let port = Port { number: port, kind };
                            emit(events, Event::Bound { service, port });
                        }
                        ServerMessage::BindFailed { service, reason } => {
                            emit(events, Event::BindFailed { service, reason });
                        }
                        ServerMessage::Ping { nonce } => {
                            if let Err(err) = send_msg(&mut control, &ClientMessage::Pong { nonce }).await {
                                break Err(err);
                            }
                        }
                        ServerMessage::Pong { .. } => {}
                        ServerMessage::Shutdown { reason } => break Err(ClientError::Shutdown(reason)),
                        ServerMessage::Welcome { .. } | ServerMessage::Denied { .. } => {
                            break Err(ClientError::Protocol("a second greeting"));
                        }
                    }
                }
                incoming = transport.accept_bi() => {
                    match incoming {
                        Ok((send, recv)) => {
                            let stream = serve_stream(send, recv, Arc::clone(&self.targets), Arc::clone(&self.udp), buffer);
                            // A yamux stream dies with the transport that opened it, so the task
                            // keeps one alive for as long as it pumps.
                            let held = Arc::clone(&transport);
                            tasks.spawn(async move {
                                stream.await;
                                drop(held);
                            });
                        }
                        Err(err) => break Err(err.into()),
                    }
                }
            }
        };

        udp_cancel.cancel();
        // Each service holds the transport through its `Sender`; on the TCP transport the
        // connection lives until the last handle drops.
        self.udp.clear();
        tasks.close();
        transport.close(CloseReason::Shutdown);
        let _ = tokio::time::timeout(DRAIN, tasks.wait()).await;
        if let Some(endpoint) = endpoint {
            endpoint.wait_idle().await;
        }
        outcome
    }

    fn register_bound(
        &self,
        service: String,
        service_id: u16,
        local: String,
        kind: Kind,
        ctx: &BindCtx<'_>,
    ) {
        if kind == Kind::Udp {
            self.udp.insert(udp::UdpLocal::new(
                service,
                service_id,
                local,
                IDLE,
                udp::SESSION_CAP,
                ctx,
            ));
        } else {
            self.targets
                .write()
                .expect("targets lock")
                .insert(service_id, Target { service, local });
        }
    }

    /// Returns the server's agent and the name it knows this client by. A refusal is reported and
    /// closed here, so a caller that sees `ClientError::Denied` has nothing left to do.
    async fn greet(
        &self,
        control: &mut Control,
        transport: &Arc<dyn Transport>,
        events: &mpsc::Sender<Event>,
    ) -> Result<(String, String), ClientError> {
        let name = self
            .cfg
            .name
            .clone()
            .or_else(|| Some(gethostname::gethostname().to_string_lossy().into_owned()));
        send_msg(
            control,
            &ClientMessage::Hello {
                name,
                agent: AGENT.to_owned(),
            },
        )
        .await?;
        match control.next::<ServerMessage>().await {
            Some(ServerMessage::Welcome { agent, client_name }) => Ok((agent, client_name)),
            Some(ServerMessage::Denied { key, .. }) => {
                emit(events, Event::Denied { key });
                transport.close(CloseReason::Denied);
                Err(ClientError::Denied(key))
            }
            Some(_) => Err(ClientError::Protocol("another message")),
            None => Err(ClientError::ControlClosed),
        }
    }

    async fn connect(&self, remote: SocketAddr) -> Result<Dialed, ClientError> {
        match self.cfg.transport.prefer {
            // `Auto` dials QUIC and nothing else while yamux delivers a reset stream as a clean
            // end-of-stream: a fallback would answer blocked UDP with a transport on which a
            // truncated transfer arrives looking complete. Deadline-free for the same reason —
            // with nothing to fall back to, a probe could only fail a handshake that would land.
            Prefer::Auto | Prefer::Quic => self.connect_quic(remote).await,
            Prefer::Tcp => self.connect_tcp(remote).await,
        }
    }

    async fn connect_quic(&self, remote: SocketAddr) -> Result<Dialed, ClientError> {
        let (tls, tuning) = self.dial_settings()?;
        let endpoint = quic::dialer(tls, tuning, remote)?;
        let transport: Arc<dyn Transport> =
            Arc::new(QuicTransport(quic::connect(&endpoint, remote).await?));
        let control = open_control(&transport).await?;
        Ok(Dialed {
            transport,
            kind: TransportKind::Quic,
            endpoint: Some(endpoint),
            control,
        })
    }

    async fn connect_tcp(&self, remote: SocketAddr) -> Result<Dialed, ClientError> {
        let (tls, tuning) = self.dial_settings()?;
        let transport: Arc<dyn Transport> = Arc::new(tcp::connect(remote, tls, tuning).await?);
        let control = open_control(&transport).await?;
        Ok(Dialed {
            transport,
            kind: TransportKind::Tcp,
            endpoint: None,
            control,
        })
    }

    fn dial_settings(&self) -> Result<(rustls::ClientConfig, Tuning), ClientError> {
        let (cert, key) = self.identity.certificate()?;
        let tls = tls::client_config(cert, key, self.cfg.server_key, tls::provider())?;
        let tuning = Tuning {
            idle_timeout: self.cfg.transport.idle_timeout,
            congestion: self.cfg.transport.congestion,
            stream_window: u32::try_from(self.cfg.transport.stream_window.0)
                .map_err(|_| ClientError::Window)?,
            connection_window: self.cfg.transport.connection_window.0,
            max_streams: Tuning::CLIENT.max_streams,
        };
        Ok((tls, tuning))
    }

    async fn send_binds(&self, control: &mut Control) -> Result<(), ClientError> {
        for (service, expose) in &self.cfg.expose {
            let (kind, port) = match expose.port {
                PortRequest::Any(kind) => (kind, None),
                PortRequest::Fixed(port) => (port.kind, Some(port.number)),
            };
            let bind = ClientMessage::Bind {
                service: service.clone(),
                kind,
                port,
                allow: expose.allow.clone(),
                proxy_protocol: expose.proxy_protocol,
            };
            send_msg(control, &bind).await?;
        }
        Ok(())
    }

    async fn resolve(&self) -> Result<SocketAddr, ClientError> {
        let (host, port) = split_host_port(&self.cfg.server)
            .ok_or_else(|| ClientError::ServerAddr(self.cfg.server.clone()))?;
        let target = format!("{host}:{}", port.unwrap_or(DEFAULT_PORT));
        let first = tokio::net::lookup_host(&target)
            .await
            .map_err(|e| ClientError::Resolve(target.clone(), e))?
            .next();
        first.ok_or(ClientError::NoAddress(target))
    }
}

/// Events are informational: awaiting a slow consumer would stall the control loop past the
/// server's liveness deadline.
fn emit(events: &mpsc::Sender<Event>, event: Event) {
    if let Err(mpsc::error::TrySendError::Full(event)) = events.try_send(event) {
        tracing::debug!(?event, "dropped an event: the receiver is behind");
    }
}

/// `Config` marks a failure a retry cannot fix — a malformed address, TLS, identity, window
/// sizing, or a message this config cannot encode. DNS failing (`Resolve`, `NoAddress`) is
/// transient, not a config problem, so it maps to `Transport` and keeps retrying. So does a dial
/// that found nothing: the server may be down, or the path blocked only for now.
fn classify(err: &ClientError) -> DisconnectCause {
    match err {
        ClientError::Shutdown(s) => DisconnectCause::Shutdown(s.clone()),
        ClientError::Unresponsive => DisconnectCause::Unresponsive,
        ClientError::Denied(_) => DisconnectCause::Denied,
        ClientError::Encode(_)
        | ClientError::ServerAddr(_)
        | ClientError::Window
        | ClientError::Tls(_)
        | ClientError::Identity(_) => DisconnectCause::Config(chain(err)),
        _ => DisconnectCause::Transport(chain(err)),
    }
}

/// A frame the message itself couldn't fill is a config bug worth giving up on; a stream the
/// peer has already closed is not, so `classify` needs the two kept apart.
async fn send_msg(control: &mut Control, msg: &ClientMessage) -> Result<(), ClientError> {
    control.send(msg).await.map_err(|err| match err {
        StreamFrameError::Io(_) => ClientError::ControlClosed,
        StreamFrameError::Frame(err) => ClientError::Encode(err),
    })
}

#[cfg(test)]
mod tests {
    use hawse_proto::frame::FrameError;

    use super::*;

    #[test]
    fn a_resolver_failure_is_worth_retrying() {
        let resolve =
            ClientError::Resolve("example.invalid:443".to_owned(), std::io::Error::other("x"));
        let no_address = ClientError::NoAddress("example.invalid:443".to_owned());
        assert!(matches!(classify(&resolve), DisconnectCause::Transport(_)));
        assert!(matches!(
            classify(&no_address),
            DisconnectCause::Transport(_)
        ));
    }

    #[test]
    fn a_dial_that_reached_nobody_is_worth_retrying() {
        let quic = ClientError::Quic(QuicError::Connection(quinn::ConnectionError::TimedOut));
        let tcp = ClientError::Transport(TransportError::Io(std::io::Error::other("x")));
        assert!(matches!(classify(&quic), DisconnectCause::Transport(_)));
        assert!(matches!(classify(&tcp), DisconnectCause::Transport(_)));
    }

    #[test]
    fn a_config_error_ends_the_retry_loop() {
        let cases = [
            ClientError::ServerAddr("bad".to_owned()),
            ClientError::Window,
            ClientError::Tls(rustls::Error::General("boom".to_owned())),
            ClientError::Identity(IdentityError::WrongAlgorithm("rsa".to_owned())),
            ClientError::Encode(FrameError::TooLarge(0)),
        ];
        for err in cases {
            assert!(
                matches!(classify(&err), DisconnectCause::Config(_)),
                "{err:?}"
            );
        }
    }
}
