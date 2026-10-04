mod backoff;
mod race;
mod udp;
mod visitor;

use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use hawse_proto::key::PublicKey;
use hawse_proto::msg::{ALPN, BindFailure, ClientMessage, ServerMessage, StreamOpen, reset};
use hawse_proto::port::{Kind, Port, PortRequest};
use quinn::Endpoint;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::config::{ClientConfig, DEFAULT_PORT, Expose, Prefer, split_host_port};
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
use backoff::{Backoff, random_unit};
use race::{Raced, race};

pub const AGENT: &str = concat!("hawse/", env!("CARGO_PKG_VERSION"));

const PING_EVERY: Duration = Duration::from_secs(15);

/// How long `Prefer::Auto` gives QUIC before it starts a TCP dial beside it: above the 99th
/// percentile of a QUIC handshake over a path losing 5% of its packets, so one lost packet does
/// not move a session onto the fallback. `docs/measurements.md` carries the measurement.
pub const FALLBACK_AFTER: Duration = Duration::from_secs(2);
const PONG_DEADLINE: Duration = Duration::from_secs(45);
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
    /// `retry_in` is how long `run` waits before reconnecting; `None` means it has stopped.
    Disconnected {
        cause: DisconnectCause,
        retry_in: Option<Duration>,
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
    #[error(
        "the server speaks a different hawse protocol than this client's {}; both ends need the same release",
        String::from_utf8_lossy(ALPN)
    )]
    Version,
    #[error(
        "cannot connect over QUIC ({}) or over the TCP fallback ({})",
        chain(.quic),
        chain(.tcp)
    )]
    NoTransport {
        quic: Box<ClientError>,
        tcp: Box<ClientError>,
    },
    #[error("stream window does not fit a QUIC window")]
    Window,
    #[error("transport buffer does not fit this machine's address space")]
    Buffer,
}

#[derive(Clone, Debug)]
pub struct Target {
    pub service: String,
    pub local: String,
    pub proxy_protocol: bool,
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

/// Bundled to keep `UdpLocal::new` under clippy's argument-count limit, and threaded through
/// `register_bound` to reach it.
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
    mut send: SendHalf,
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
        Err(err) => {
            tracing::debug!(err = %chain(&err), "bad stream header");
            visitor::refuse(&mut send, &mut recv, reset::UNEXPECTED_STREAM).await;
        }
    }
}

pub struct Client {
    cfg: ClientConfig,
    identity: Identity,
    targets: Targets,
    udp: Arc<udp::Registry>,
    fallback_after: Duration,
}

impl Client {
    pub fn new(cfg: ClientConfig, identity: Identity) -> Self {
        Self {
            cfg,
            identity,
            targets: Arc::default(),
            udp: Arc::default(),
            fallback_after: FALLBACK_AFTER,
        }
    }

    /// Replaces `FALLBACK_AFTER`. No config file reaches this: it is here so tests do not wait
    /// whole seconds for a fallback.
    #[must_use]
    pub fn with_fallback_after(mut self, after: Duration) -> Self {
        self.fallback_after = after;
        self
    }

    /// Reconnects after every failure, backing off from 1 s to 30 s, until `cancel` fires,
    /// except a `Config` cause, which retrying cannot fix and ends it instead.
    pub async fn run(&self, cancel: CancellationToken, events: mpsc::Sender<Event>) {
        let mut backoff = Backoff::default();
        while !cancel.is_cancelled() {
            let mut welcomed = None;
            let wait = match self.session(cancel.clone(), &events, &mut welcomed).await {
                Ok(()) => return,
                // The greeting is not cancel-aware, so a failure can land after shutdown began;
                // announcing a retry then would promise one that never comes.
                Err(_) if cancel.is_cancelled() => return,
                Err(err) => {
                    let cause = classify(&err);
                    let lasted = welcomed.map_or(Duration::ZERO, |at| at.elapsed());
                    let retry_in = (!matches!(cause, DisconnectCause::Config(_)))
                        .then(|| backoff.next(lasted, random_unit()));
                    emit(&events, Event::Disconnected { cause, retry_in });
                    match retry_in {
                        Some(wait) => wait,
                        None => return,
                    }
                }
            };
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// One session. `Ok` means `cancel` ended it; every other ending is an error.
    pub async fn run_once(
        &self,
        cancel: CancellationToken,
        events: &mpsc::Sender<Event>,
    ) -> Result<(), ClientError> {
        self.session(cancel, events, &mut None).await
    }

    /// `run_once`, also recording in `welcomed` when the server accepted the session, which is
    /// what `run`'s backoff measures a session's life from.
    async fn session(
        &self,
        cancel: CancellationToken,
        events: &mpsc::Sender<Event>,
        welcomed: &mut Option<Instant>,
    ) -> Result<(), ClientError> {
        let buffer =
            usize::try_from(self.cfg.transport.buffer.0).map_err(|_| ClientError::Buffer)?;
        let dial = async {
            let (target, addrs) = self.resolve().await?;
            let connect = |remote| async move {
                let dialed = self.connect(remote).await;
                if let Err(err) = &dialed {
                    tracing::debug!(%remote, err = %chain(err), "this address did not connect");
                }
                dialed
            };
            dial_each(addrs, ClientError::NoAddress(target), connect).await
        };
        // Left to itself a dial nobody answers runs on to the idle timeout, and a stop would wait
        // out all of it.
        let (remote, dialed) = tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            dialed = dial => dialed?,
        };
        self.serve(remote, dialed, buffer, cancel, events, welcomed)
            .await
    }

    /// The session from its greeting on, over a transport already dialed.
    async fn serve(
        &self,
        remote: SocketAddr,
        dialed: Dialed,
        buffer: usize,
        cancel: CancellationToken,
        events: &mpsc::Sender<Event>,
        welcomed: &mut Option<Instant>,
    ) -> Result<(), ClientError> {
        let Dialed {
            transport,
            kind,
            endpoint,
            mut control,
        } = dialed;

        let (agent, client_name) = self.greet(&mut control, &transport, events).await?;
        *welcomed = Some(Instant::now());
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
        let mut ping = ping_interval();
        let mut last_heard = tokio::time::Instant::now();
        let mut nonce = 0u64;

        let outcome = loop {
            tokio::select! {
                () = cancel.cancelled() => break Ok(()),
                () = tokio::time::sleep_until(last_heard + PONG_DEADLINE) => {
                    break Err(ClientError::Unresponsive);
                }
                _ = ping.tick() => {
                    nonce += 1;
                    if let Err(err) = send_msg(&mut control, &ClientMessage::Ping { nonce }).await {
                        break Err(err);
                    }
                }
                msg = control.next::<ServerMessage>() => {
                    let Some(msg) = msg else { break Err(ClientError::ControlClosed) };
                    last_heard = tokio::time::Instant::now();
                    match msg {
                        ServerMessage::Bound { service, service_id, port, address } => {
                            let Some(expose) = self.cfg.expose.get(&service) else {
                                tracing::warn!(service, "server bound a service we never asked for");
                                continue;
                            };
                            let kind = expose.port.kind();
                            self.register_bound(service.clone(), service_id, expose, listener_addr(address, remote, port), &bind_ctx);
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
        expose: &Expose,
        listener: SocketAddr,
        ctx: &BindCtx<'_>,
    ) {
        let local = expose.local.clone();
        if expose.port.kind() == Kind::Udp {
            self.udp.insert(udp::UdpLocal::new(
                service,
                service_id,
                local,
                IDLE,
                udp::SESSION_CAP,
                expose.proxy_protocol.then_some(listener),
                ctx,
            ));
        } else {
            self.targets.write().expect("targets lock").insert(
                service_id,
                Target {
                    service,
                    local,
                    proxy_protocol: expose.proxy_protocol,
                },
            );
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
        let dialed = match self.cfg.transport.prefer {
            Prefer::Auto => self.connect_auto(remote).await,
            Prefer::Quic => self.connect_quic(remote).await,
            Prefer::Tcp => self.connect_tcp(remote).await,
        };
        dialed.map_err(versioned)
    }

    /// Neither dial sends a `Hello`, so the one that loses never becomes a session on the server.
    async fn connect_auto(&self, remote: SocketAddr) -> Result<Dialed, ClientError> {
        let raced = race(
            self.connect_quic(remote),
            || self.connect_tcp(remote),
            self.fallback_after,
        )
        .await;
        match raced {
            Raced::Quic(dialed) => Ok(dialed),
            Raced::Tcp { dialed, quic } => {
                if let Some(err) = quic {
                    tracing::warn!(
                        err = %chain(&err),
                        "QUIC failed, so this session is on the TCP fallback until it ends; each UDP service's traffic shares one stream there"
                    );
                } else {
                    tracing::warn!(
                        after = ?self.fallback_after,
                        "QUIC had not connected, so this session is on the TCP fallback until it ends; each UDP service's traffic shares one stream there"
                    );
                }
                Ok(dialed)
            }
            Raced::Failed { quic, tcp } => Err(ClientError::NoTransport {
                quic: Box::new(quic),
                tcp: Box::new(tcp),
            }),
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

    /// The `host:port` that was looked up, and its addresses in the resolver's order.
    async fn resolve(&self) -> Result<(String, Vec<SocketAddr>), ClientError> {
        let (host, port) = split_host_port(&self.cfg.server)
            .ok_or_else(|| ClientError::ServerAddr(self.cfg.server.clone()))?;
        let target = format!("{host}:{}", port.unwrap_or(DEFAULT_PORT));
        let addrs = tokio::net::lookup_host(&target)
            .await
            .map_err(|e| ClientError::Resolve(target.clone(), e))?
            .collect();
        Ok((target, addrs))
    }
}

/// Dials each address in turn and returns the first that connects, with the address it connected
/// to. When none does the error is the last dial's, or `none` when there was nothing to dial.
async fn dial_each<T, E, F, C>(
    addrs: impl IntoIterator<Item = SocketAddr>,
    none: E,
    dial: F,
) -> Result<(SocketAddr, T), E>
where
    F: Fn(SocketAddr) -> C,
    C: Future<Output = Result<T, E>>,
{
    let mut failed = none;
    for remote in addrs {
        match dial(remote).await {
            Ok(dialed) => return Ok((remote, dialed)),
            Err(err) => failed = err,
        }
    }
    Err(failed)
}

/// Events are informational: awaiting a slow consumer would stall the control loop past the
/// server's liveness deadline.
fn emit(events: &mpsc::Sender<Event>, event: Event) {
    if let Err(mpsc::error::TrySendError::Full(event)) = events.try_send(event) {
        tracing::debug!(?event, "dropped an event: the receiver is behind");
    }
}

/// The address a PROXY header names as the destination. A wildcard bind names no address, so it is
/// the one this client dialed.
fn listener_addr(bound: IpAddr, dialed: SocketAddr, port: u16) -> SocketAddr {
    let ip = if bound.is_unspecified() {
        dialed.ip()
    } else {
        bound
    };
    SocketAddr::new(ip.to_canonical(), port)
}

/// Whether a dial failed because the peer offers no protocol this build speaks. TLS says so with
/// alert 120, which QUIC carries as a crypto error and TLS over TCP as the alert itself.
fn wrong_version(err: &ClientError) -> bool {
    const NO_APPLICATION_PROTOCOL: u8 = 120;
    let refused = quinn::TransportErrorCode::crypto(NO_APPLICATION_PROTOCOL);
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(err) = source {
        match err.downcast_ref::<quinn::ConnectionError>() {
            Some(quinn::ConnectionError::ConnectionClosed(close))
                if close.error_code == refused =>
            {
                return true;
            }
            Some(quinn::ConnectionError::TransportError(failed)) if failed.code == refused => {
                return true;
            }
            _ => {}
        }
        // An `io::Error` reports its inner error's source as its own, skipping the inner error:
        // tokio-rustls wraps the alert that way, so it has to be reached through `get_ref`.
        let inner = err
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::get_ref)
            .and_then(|inner| inner.downcast_ref::<rustls::Error>());
        if matches!(
            inner,
            Some(rustls::Error::AlertReceived(
                rustls::AlertDescription::NoApplicationProtocol
            ))
        ) {
            return true;
        }
        source = err.source();
    }
    false
}

/// A dial that failed on the protocol version says so, whichever transport met it.
fn versioned(err: ClientError) -> ClientError {
    let mismatch = match &err {
        ClientError::NoTransport { quic, tcp } => wrong_version(quic) || wrong_version(tcp),
        other => wrong_version(other),
    };
    if mismatch { ClientError::Version } else { err }
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
        | ClientError::Buffer
        | ClientError::Tls(_)
        | ClientError::Identity(_) => DisconnectCause::Config(chain(err)),
        ClientError::NoTransport { quic, tcp }
            if matches!(classify(quic), DisconnectCause::Config(_))
                && matches!(classify(tcp), DisconnectCause::Config(_)) =>
        {
            DisconnectCause::Config(chain(err))
        }
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
    use std::cell::RefCell;

    use hawse_proto::frame::FrameError;

    use super::*;

    fn no_protocol_alert() -> ClientError {
        let alert = rustls::Error::AlertReceived(rustls::AlertDescription::NoApplicationProtocol);
        let io = std::io::Error::new(std::io::ErrorKind::InvalidData, alert);
        ClientError::Transport(TransportError::Connection(Box::new(io)))
    }

    fn addr(host: u8) -> SocketAddr {
        SocketAddr::from(([192, 0, 2, host], 4433))
    }

    #[tokio::test]
    async fn a_dial_moves_on_to_the_next_address_when_one_fails() {
        let tried = RefCell::new(Vec::new());
        let dial = |remote| {
            tried.borrow_mut().push(remote);
            async move {
                if remote == addr(2) {
                    Ok("up")
                } else {
                    Err("refused")
                }
            }
        };
        let dialed = dial_each([addr(1), addr(2), addr(3)], "no address", dial).await;
        assert_eq!(dialed, Ok((addr(2), "up")));
        assert_eq!(*tried.borrow(), [addr(1), addr(2)]);
    }

    #[tokio::test]
    async fn a_dial_that_fails_at_every_address_reports_the_last_failure() {
        let dial = |remote| async move { Err::<&str, _>(format!("{remote} refused")) };
        let none = || "no address".to_owned();
        assert_eq!(
            dial_each([addr(1), addr(2)], none(), dial).await,
            Err("192.0.2.2:4433 refused".to_owned())
        );
        assert_eq!(dial_each([], none(), dial).await, Err(none()));
    }

    #[test]
    fn a_mismatch_on_either_dial_is_a_version_error() {
        let timed_out =
            || ClientError::Quic(QuicError::Connection(quinn::ConnectionError::TimedOut));
        let both = ClientError::NoTransport {
            quic: Box::new(timed_out()),
            tcp: Box::new(no_protocol_alert()),
        };
        assert!(matches!(versioned(both), ClientError::Version));
        let neither = ClientError::NoTransport {
            quic: Box::new(timed_out()),
            tcp: Box::new(timed_out()),
        };
        assert!(matches!(
            versioned(neither),
            ClientError::NoTransport { .. }
        ));
    }

    #[test]
    fn a_dial_that_failed_both_ways_says_both_and_is_worth_retrying() {
        let err = ClientError::NoTransport {
            quic: Box::new(ClientError::Unresponsive),
            tcp: Box::new(ClientError::ControlClosed),
        };
        let text = err.to_string();
        assert!(text.contains("server stopped answering"), "{text}");
        assert!(text.contains("control stream closed"), "{text}");
        assert!(matches!(classify(&err), DisconnectCause::Transport(_)));
    }

    #[test]
    fn a_dial_that_failed_both_ways_for_the_config_is_not_retried() {
        let both = ClientError::NoTransport {
            quic: Box::new(ClientError::Window),
            tcp: Box::new(ClientError::Window),
        };
        assert!(matches!(classify(&both), DisconnectCause::Config(_)));
        let one = ClientError::NoTransport {
            quic: Box::new(ClientError::Window),
            tcp: Box::new(ClientError::ControlClosed),
        };
        assert!(matches!(classify(&one), DisconnectCause::Transport(_)));
    }

    #[test]
    fn the_no_protocol_alert_reads_as_a_version_mismatch() {
        assert!(wrong_version(&no_protocol_alert()));
        let timed_out = ClientError::Quic(QuicError::Connection(quinn::ConnectionError::TimedOut));
        assert!(!wrong_version(&timed_out));
        assert!(matches!(
            versioned(no_protocol_alert()),
            ClientError::Version
        ));
        assert!(matches!(versioned(timed_out), ClientError::Quic(_)));
    }

    #[test]
    fn a_version_mismatch_is_worth_retrying() {
        assert!(matches!(
            classify(&ClientError::Version),
            DisconnectCause::Transport(_)
        ));
    }

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
            ClientError::Buffer,
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
