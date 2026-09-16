mod visitor;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use hawse_proto::frame::{codec, decode, encode};
use hawse_proto::key::PublicKey;
use hawse_proto::msg::{BindFailure, ClientMessage, ServerMessage};
use hawse_proto::port::{Port, PortRequest};
use quinn::{Connection, Endpoint, RecvStream, SendStream, VarInt};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::config::{ClientConfig, DEFAULT_PORT, split_host_port};
use crate::error::chain;
use crate::identity::{Identity, IdentityError};
use crate::tls;
use crate::transport::quic::{self, QuicError, Tuning};

pub const AGENT: &str = concat!("hawse/", env!("CARGO_PKG_VERSION"));

const PING_EVERY: Duration = Duration::from_secs(15);
const PONG_DEADLINE: Duration = Duration::from_secs(45);
const RETRY_AFTER: Duration = Duration::from_secs(5);
const DRAIN: Duration = Duration::from_secs(2);

type Tx = FramedWrite<SendStream, LengthDelimitedCodec>;
type Rx = FramedRead<RecvStream, LengthDelimitedCodec>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Connected {
        remote: SocketAddr,
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

pub struct Client {
    cfg: ClientConfig,
    identity: Identity,
    targets: Targets,
}

impl Client {
    pub fn new(cfg: ClientConfig, identity: Identity) -> Self {
        Self {
            cfg,
            identity,
            targets: Arc::default(),
        }
    }

    /// Reconnects 5 s after every failure, including `Denied`, until `cancel` fires.
    /// A config-class failure ends it instead, since retrying cannot fix one.
    pub async fn run(&self, cancel: CancellationToken, events: mpsc::Sender<Event>) {
        while !cancel.is_cancelled() {
            match self.run_once(cancel.clone(), &events).await {
                Ok(()) => return,
                Err(err) => {
                    let (fatal, cause) = classify(&err);
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
        let (endpoint, conn, mut tx, mut rx) = self.connect(remote).await?;

        let name = self
            .cfg
            .name
            .clone()
            .or_else(|| Some(gethostname::gethostname().to_string_lossy().into_owned()));
        send_msg(
            &mut tx,
            &ClientMessage::Hello {
                name,
                agent: AGENT.to_owned(),
            },
        )
        .await?;

        let (agent, client_name) = match next(&mut rx).await {
            Some(ServerMessage::Welcome { agent, client_name }) => (agent, client_name),
            Some(ServerMessage::Denied { key, .. }) => {
                emit(events, Event::Denied { key });
                conn.close(VarInt::from_u32(0), b"denied");
                return Err(ClientError::Denied(key));
            }
            Some(_) => return Err(ClientError::Protocol("another message")),
            None => return Err(ClientError::ControlClosed),
        };
        emit(
            events,
            Event::Connected {
                remote,
                name: client_name,
                agent,
            },
        );

        self.send_binds(&mut tx).await?;

        self.targets.write().expect("targets lock").clear();
        let tasks = TaskTracker::new();
        let buffer = usize::try_from(self.cfg.transport.buffer.0).expect("a validated buffer");
        let mut ping =
            tokio::time::interval_at(tokio::time::Instant::now() + PING_EVERY, PING_EVERY);
        ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
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
                    if let Err(err) = send_msg(&mut tx, &ClientMessage::Ping { nonce }).await {
                        break Err(err);
                    }
                }
                msg = next(&mut rx) => {
                    let Some(msg) = msg else { break Err(ClientError::ControlClosed) };
                    last_heard = Instant::now();
                    match msg {
                        ServerMessage::Bound { service, service_id, port } => {
                            let Some(expose) = self.cfg.expose.get(&service) else {
                                tracing::warn!(service, "server bound a service we never asked for");
                                continue;
                            };
                            let target = Target { service: service.clone(), local: expose.local.clone() };
                            self.targets.write().expect("targets lock").insert(service_id, target);
                            let port = Port { number: port, kind: expose.port.kind() };
                            emit(events, Event::Bound { service, port });
                        }
                        ServerMessage::BindFailed { service, reason } => {
                            emit(events, Event::BindFailed { service, reason });
                        }
                        ServerMessage::Ping { nonce } => {
                            if let Err(err) = send_msg(&mut tx, &ClientMessage::Pong { nonce }).await {
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
                incoming = conn.accept_bi() => {
                    match incoming {
                        Ok((send, recv)) => {
                            tasks.spawn(visitor::serve(send, recv, Arc::clone(&self.targets), buffer));
                        }
                        Err(err) => break Err(QuicError::from(err).into()),
                    }
                }
            }
        };

        tasks.close();
        conn.close(VarInt::from_u32(0), b"bye");
        let _ = tokio::time::timeout(DRAIN, tasks.wait()).await;
        endpoint.wait_idle().await;
        outcome
    }

    async fn connect(
        &self,
        remote: SocketAddr,
    ) -> Result<(Endpoint, Connection, Tx, Rx), ClientError> {
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
        let endpoint = quic::dialer(tls, tuning, remote)?;
        let conn = quic::connect(&endpoint, remote).await?;
        let (send, recv) = conn.open_bi().await.map_err(QuicError::from)?;
        let tx = FramedWrite::new(send, codec());
        let rx = FramedRead::new(recv, codec());
        Ok((endpoint, conn, tx, rx))
    }

    async fn send_binds(&self, tx: &mut Tx) -> Result<(), ClientError> {
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
            send_msg(tx, &bind).await?;
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

/// A bad address, DNS, TLS, identity, window sizing, or a message this config cannot
/// encode is not something a reconnect could ever fix, so those are fatal.
fn classify(err: &ClientError) -> (bool, DisconnectCause) {
    match err {
        ClientError::Shutdown(s) => (false, DisconnectCause::Shutdown(s.clone())),
        ClientError::Unresponsive => (false, DisconnectCause::Unresponsive),
        ClientError::Denied(_) => (false, DisconnectCause::Denied),
        ClientError::Encode(_)
        | ClientError::ServerAddr(_)
        | ClientError::Resolve(..)
        | ClientError::NoAddress(_)
        | ClientError::Window
        | ClientError::Tls(_)
        | ClientError::Identity(_) => (true, DisconnectCause::Config(chain(err))),
        _ => (false, DisconnectCause::Transport(chain(err))),
    }
}

async fn send_msg(tx: &mut Tx, msg: &ClientMessage) -> Result<(), ClientError> {
    let bytes = encode(msg).map_err(ClientError::Encode)?;
    tx.send(bytes).await.map_err(|_| ClientError::ControlClosed)
}

async fn next(rx: &mut Rx) -> Option<ServerMessage> {
    let frame = rx.next().await?.ok()?;
    decode(&frame).ok()
}
