use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use hawse_proto::frame::{codec, decode, encode};
use hawse_proto::msg::{BindFailure, ClientMessage, DenyReason, ServerMessage};
use hawse_proto::name;
use hawse_proto::port::{Kind, Port};
use ipnet::IpNet;
use quinn::{Connection, RecvStream, SendStream, VarInt};
use tokio::time::MissedTickBehavior;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::Instrument as _;

use super::policy::Grant;
use super::{AGENT, DRAIN, Shared, listener};
use crate::net;
use crate::transport::quic;

const PING_EVERY: Duration = Duration::from_secs(15);
const PONG_DEADLINE: Duration = Duration::from_secs(45);
const DENIED_LINGER: Duration = Duration::from_secs(2);
const HELLO_DEADLINE: Duration = Duration::from_secs(10);

type Tx = FramedWrite<SendStream, LengthDelimitedCodec>;
type Rx = FramedRead<RecvStream, LengthDelimitedCodec>;

pub async fn run(conn: Connection, shared: Arc<Shared>, cancel: CancellationToken) {
    let remote = conn.remote_address();
    let Some(key) = quic::peer_key(&conn) else {
        conn.close(VarInt::from_u32(2), b"no key");
        return;
    };
    // Nothing here is authorized yet, so a peer that never speaks must not hold a task open or stall shutdown.
    let greeting = async {
        let (control_send, control_recv) = conn.accept_bi().await?;
        let tx = FramedWrite::new(control_send, codec());
        let mut rx = FramedRead::new(control_recv, codec());
        let hello = next(&mut rx).await;
        Ok::<_, quinn::ConnectionError>((tx, rx, hello))
    };
    let greeted = tokio::select! {
        () = cancel.cancelled() => {
            conn.close(VarInt::from_u32(0), b"shutdown");
            return;
        }
        greeted = tokio::time::timeout(HELLO_DEADLINE, greeting) => greeted,
    };
    let Ok(opened) = greeted else {
        conn.close(VarInt::from_u32(4), b"no hello");
        return;
    };
    let Ok((mut tx, rx, hello)) = opened else {
        return;
    };
    let Some(ClientMessage::Hello { agent, .. }) = hello else {
        conn.close(VarInt::from_u32(3), b"expected hello");
        return;
    };

    let Some(grant) = shared.policy.lookup(&key).cloned() else {
        tracing::info!(%key, %remote, "denied unknown key. authorize it with: hawse authorize {key} --name NAME");
        let _ = send(
            &mut tx,
            &ServerMessage::Denied {
                reason: DenyReason::UnknownKey,
                key,
            },
        )
        .await;
        // `close` discards unsent stream data, so let the client read the denial first.
        let _ = tx.into_inner().finish();
        tokio::select! {
            () = cancel.cancelled() => {}
            _ = tokio::time::timeout(DENIED_LINGER, conn.closed()) => {}
        }
        conn.close(VarInt::from_u32(1), b"denied");
        return;
    };

    let span = tracing::info_span!("session", client = %grant.name, %remote);
    async move {
        let welcome = ServerMessage::Welcome {
            agent: AGENT.to_owned(),
            client_name: grant.name.clone(),
        };
        if send(&mut tx, &welcome).await.is_err() {
            return;
        }
        tracing::info!(%agent, "client connected");
        let session = Session {
            conn,
            shared,
            grant,
            services: HashMap::new(),
            next_id: 1,
            tasks: TaskTracker::new(),
        };
        session.serve(tx, rx, cancel).await;
    }
    .instrument(span)
    .await;
}

struct Session {
    conn: Connection,
    shared: Arc<Shared>,
    grant: Grant,
    services: HashMap<String, BoundService>,
    next_id: u16,
    tasks: TaskTracker,
}

struct BoundService {
    port: Port,
    cancel: CancellationToken,
}

impl Session {
    async fn serve(mut self, mut tx: Tx, mut rx: Rx, cancel: CancellationToken) {
        let mut ping = tokio::time::interval(PING_EVERY);
        ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut last_heard = Instant::now();
        let mut nonce = 0u64;

        let reason = loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    let _ = send(&mut tx, &ServerMessage::Shutdown { reason: "server shutting down".to_owned() }).await;
                    break "shutdown";
                }
                _ = ping.tick() => {
                    if last_heard.elapsed() > PONG_DEADLINE {
                        break "unresponsive";
                    }
                    nonce += 1;
                    if send(&mut tx, &ServerMessage::Ping { nonce }).await.is_err() {
                        break "control stream closed";
                    }
                }
                msg = next(&mut rx) => {
                    let Some(msg) = msg else { break "client left" };
                    last_heard = Instant::now();
                    let reply = match msg {
                        ClientMessage::Bind { service, kind, port, allow, proxy_protocol } => {
                            Some(self.bind(&service, kind, port, &allow, proxy_protocol))
                        }
                        ClientMessage::Unbind { service } => {
                            self.unbind(&service);
                            None
                        }
                        ClientMessage::Ping { nonce } => Some(ServerMessage::Pong { nonce }),
                        ClientMessage::Pong { .. } => None,
                        ClientMessage::Hello { .. } => break "duplicate hello",
                    };
                    if let Some(reply) = reply
                        && send(&mut tx, &reply).await.is_err()
                    {
                        break "control stream closed";
                    }
                }
            }
        };

        tracing::info!(reason, "session ended");
        let names: Vec<String> = self.services.keys().cloned().collect();
        for name in names {
            self.unbind(&name);
        }
        self.tasks.close();
        let _ = tokio::time::timeout(DRAIN, self.tasks.wait()).await;
        let _ = tx.into_inner().finish();
        self.conn.close(VarInt::from_u32(0), reason.as_bytes());
    }

    fn bind(
        &mut self,
        service: &str,
        kind: Kind,
        port: Option<u16>,
        allow: &[IpNet],
        proxy_protocol: bool,
    ) -> ServerMessage {
        let failed = |reason: BindFailure| ServerMessage::BindFailed {
            service: service.to_owned(),
            reason,
        };
        if name::validate(service).is_err() {
            return failed(BindFailure::BadPort);
        }
        if self.services.contains_key(service) {
            return failed(BindFailure::InUse);
        }
        if kind == Kind::Udp {
            tracing::warn!(service, "udp services are not supported yet");
            return failed(BindFailure::BadPort);
        }
        // Ignoring these would open a port with weaker guarantees than the client asked for.
        if !allow.is_empty() {
            tracing::warn!(service, "allow lists are not supported yet");
            return failed(BindFailure::BadPort);
        }
        if proxy_protocol {
            tracing::warn!(service, "proxy protocol is not supported yet");
            return failed(BindFailure::BadPort);
        }
        let claimed = {
            let mut ports = self.shared.ports.lock().expect("port allocator lock");
            match port {
                Some(number) => {
                    let port = Port { number, kind };
                    if !self.grant.allows(port) {
                        return failed(BindFailure::NotGranted);
                    }
                    ports.claim(port).map(|()| port)
                }
                None => ports.claim_dynamic(kind).ok_or(BindFailure::InUse),
            }
        };
        let port = match claimed {
            Ok(port) => port,
            Err(reason) => return failed(reason),
        };
        let listener = match net::bind_tcp(port.number) {
            Ok(listener) => listener,
            Err(err) => {
                tracing::warn!(service, %port, %err, "cannot bind");
                self.release(port);
                return failed(BindFailure::InUse);
            }
        };
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let cancel = CancellationToken::new();
        self.tasks.spawn(listener::serve(
            self.conn.clone(),
            listener,
            id,
            self.shared.buffer,
            cancel.clone(),
            self.tasks.clone(),
        ));
        self.services
            .insert(service.to_owned(), BoundService { port, cancel });
        tracing::info!(service, %port, "bound");
        ServerMessage::Bound {
            service: service.to_owned(),
            service_id: id,
            port: port.number,
        }
    }

    fn unbind(&mut self, service: &str) {
        if let Some(bound) = self.services.remove(service) {
            bound.cancel.cancel();
            self.release(bound.port);
            tracing::info!(service, port = %bound.port, "unbound");
        }
    }

    fn release(&self, port: Port) {
        self.shared
            .ports
            .lock()
            .expect("port allocator lock")
            .release(port);
    }
}

async fn send(tx: &mut Tx, msg: &ServerMessage) -> Result<(), ()> {
    let bytes = encode(msg).map_err(|_| ())?;
    tx.send(bytes).await.map_err(|_| ())
}

async fn next(rx: &mut Rx) -> Option<ClientMessage> {
    let frame = rx.next().await?.ok()?;
    decode(&frame).ok()
}
