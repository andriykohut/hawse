mod visitor;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use hawse_proto::key::PublicKey;
use hawse_proto::msg::{BindFailure, ClientMessage, ServerMessage};
use hawse_proto::port::{Port, PortRequest};
use quinn::{Connection, Endpoint, VarInt};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::config::{ClientConfig, DEFAULT_PORT, split_host_port};
use crate::control::Control;
use crate::error::chain;
use crate::frame::StreamFrameError;
use crate::identity::{Identity, IdentityError};
use crate::tls;
use crate::transport::quic::{self, QuicError, Tuning};
use crate::transport::{RecvHalf, SendHalf};

pub const AGENT: &str = concat!("hawse/", env!("CARGO_PKG_VERSION"));

const PING_EVERY: Duration = Duration::from_secs(15);
const PONG_DEADLINE: Duration = Duration::from_secs(45);
const RETRY_AFTER: Duration = Duration::from_secs(5);
const DRAIN: Duration = Duration::from_secs(2);

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
        let (endpoint, conn, mut control) = self.connect(remote).await?;

        let name = self
            .cfg
            .name
            .clone()
            .or_else(|| Some(gethostname::gethostname().to_string_lossy().into_owned()));
        send_msg(
            &mut control,
            &ClientMessage::Hello {
                name,
                agent: AGENT.to_owned(),
            },
        )
        .await?;

        let (agent, client_name) = match control.next::<ServerMessage>().await {
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

        self.send_binds(&mut control).await?;

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
                            let target = Target { service: service.clone(), local: expose.local.clone() };
                            self.targets.write().expect("targets lock").insert(service_id, target);
                            let port = Port { number: port, kind: expose.port.kind() };
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
    ) -> Result<(Endpoint, Connection, Control), ClientError> {
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
        let control = Control::new(SendHalf::Quic(send), RecvHalf::Quic(recv));
        Ok((endpoint, conn, control))
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
/// transient, not a config problem, so it maps to `Transport` and keeps retrying.
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
