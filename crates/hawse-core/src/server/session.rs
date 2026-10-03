use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hawse_proto::key::PublicKey;
use hawse_proto::msg::{BindFailure, ClientMessage, DenyReason, ServerMessage, reset};
use hawse_proto::name;
use hawse_proto::port::{Kind, Port};
use ipnet::IpNet;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::Instrument as _;

use super::policy::Grant;
use super::udp::{self, UdpService, UdpServices};
use super::{AGENT, Live, Shared, listener};
use crate::allow::AllowList;
use crate::control::Control;
use crate::net;
use crate::transport::{CloseReason, Transport, TransportError};
use crate::udp::FINISH_WAIT;

const PING_EVERY: Duration = Duration::from_secs(15);
const PONG_DEADLINE: Duration = Duration::from_secs(45);
const DENIED_LINGER: Duration = Duration::from_secs(2);
const HELLO_DEADLINE: Duration = Duration::from_secs(10);
const SHUTDOWN_LINGER: Duration = Duration::from_secs(2);
/// Under the server's own 5 s drain, so a session's close still lands inside it.
const DRAIN: Duration = Duration::from_secs(4);
const SUPERSEDE_WAIT: Duration = Duration::from_secs(5);

/// `remote` is the address the handshake proved, not the connection's current one: a QUIC path
/// can migrate to a new, unproven source before it is validated, and a failure charged there would
/// limit whoever that source names.
pub async fn run(
    transport: Arc<dyn Transport>,
    remote: SocketAddr,
    shared: Arc<Shared>,
    cancel: CancellationToken,
) {
    let Some(key) = transport.peer_key() else {
        shared.auth_failed(remote.ip());
        transport.close(CloseReason::NoKey);
        return;
    };
    // TLS has proved the key by here, so a known one whose link drops before its Hello is a client
    // on a bad network rather than a stranger, and counting it could lock the client out.
    let stranger = shared.policy.lookup(&key).is_none();
    // Nothing here is authorized yet, so a peer that never speaks must not hold a task open or stall shutdown.
    let greeting = async {
        let (control_send, control_recv) = transport.accept_bi().await?;
        let mut control = Control::new(control_send, control_recv);
        let hello = control.next::<ClientMessage>().await;
        Ok::<_, TransportError>((control, hello))
    };
    let greeted = tokio::select! {
        () = cancel.cancelled() => {
            transport.close(CloseReason::Shutdown);
            return;
        }
        greeted = tokio::time::timeout(HELLO_DEADLINE, greeting) => greeted,
    };
    let Ok(opened) = greeted else {
        if stranger {
            shared.auth_failed(remote.ip());
        }
        transport.close(CloseReason::NoHello);
        return;
    };
    let Ok((mut control, hello)) = opened else {
        if stranger {
            shared.auth_failed(remote.ip());
        }
        return;
    };
    let Some(ClientMessage::Hello { agent, .. }) = hello else {
        if stranger {
            shared.auth_failed(remote.ip());
        }
        transport.close(CloseReason::BadHello);
        return;
    };

    let Some(grant) = shared.policy.lookup(&key).cloned() else {
        shared.auth_failed(remote.ip());
        tracing::info!(%key, %remote, "denied unknown key. authorize it with: hawse authorize {key} --name NAME");
        let _ = control
            .send(&ServerMessage::Denied {
                reason: DenyReason::UnknownKey,
                key,
            })
            .await;
        // A QUIC `close` discards unsent stream data, so let the client read the denial first.
        control.finish().await;
        tokio::select! {
            () = cancel.cancelled() => {}
            _ = tokio::time::timeout(DENIED_LINGER, transport.closed()) => {}
        }
        transport.close(CloseReason::Denied);
        return;
    };

    let span = tracing::info_span!("session", client = %grant.name, %remote);
    async move {
        let live = Arc::new(Live {
            cancel: cancel.child_token(),
            done: CancellationToken::new(),
            remote,
        });
        let previous = shared
            .sessions
            .lock()
            .expect("sessions lock")
            .insert(key, Arc::clone(&live));
        if let Some(previous) = previous {
            previous.cancel.cancel();
            tracing::info!(previous = %previous.remote, "superseding this key's previous session");
            let _ = tokio::time::timeout(SUPERSEDE_WAIT, previous.done.cancelled()).await;
        }
        let welcome = ServerMessage::Welcome {
            agent: AGENT.to_owned(),
            client_name: grant.name.clone(),
        };
        if control.send(&welcome).await.is_err() {
            retire(&shared, key, &live);
            return;
        }
        tracing::info!(%agent, "client connected");
        let session = Session {
            transport,
            shared,
            grant,
            key,
            live,
            services: HashMap::new(),
            ids: ServiceIds::new(),
            tasks: TaskTracker::new(),
            udp: Arc::default(),
        };
        session.serve(control, cancel).await;
    }
    .instrument(span)
    .await;
}

/// Signals that this session's ports are free, then drops its claim on the key unless a newer
/// session has already taken it.
fn retire(shared: &Shared, key: PublicKey, live: &Arc<Live>) {
    live.done.cancel();
    let mut sessions = shared.sessions.lock().expect("sessions lock");
    if sessions
        .get(&key)
        .is_some_and(|held| Arc::ptr_eq(held, live))
    {
        sessions.remove(&key);
    }
}

/// Our client opens no stream after the control stream. One that arrives anyway is refused at once,
/// so it neither waits unread nor holds a slot in the stream budget.
async fn refuse_streams(transport: Arc<dyn Transport>, cancel: CancellationToken) {
    let mut logged = false;
    loop {
        let (mut send, mut recv) = tokio::select! {
            () = cancel.cancelled() => return,
            accepted = transport.accept_bi() => match accepted {
                Ok(halves) => halves,
                Err(_) => return,
            },
        };
        if !logged {
            tracing::debug!("refusing a stream the client opened");
            logged = true;
        }
        // Raced against the shutdown: a stream on a stalled link must not hold the session's drain.
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = tokio::time::timeout(FINISH_WAIT, send.reset_flushed(reset::UNEXPECTED_STREAM)) => {}
        }
        recv.stop(reset::UNEXPECTED_STREAM);
    }
}

struct Session {
    transport: Arc<dyn Transport>,
    shared: Arc<Shared>,
    grant: Grant,
    key: PublicKey,
    live: Arc<Live>,
    services: HashMap<String, BoundService>,
    ids: ServiceIds,
    tasks: TaskTracker,
    udp: UdpServices,
}

struct BoundService {
    id: u16,
    port: Port,
    cancel: CancellationToken,
}

/// Hands out ids in order, skipping any a live service still holds: past a wrap the
/// counter would otherwise reissue an id the client is still routing visitors on.
#[derive(Debug)]
struct ServiceIds {
    next: u16,
}

impl ServiceIds {
    fn new() -> Self {
        Self { next: 1 }
    }

    fn claim(&mut self, live: &HashSet<u16>) -> Option<u16> {
        for _ in 0..u16::MAX {
            let id = self.next;
            self.next = if id == u16::MAX { 1 } else { id + 1 };
            if !live.contains(&id) {
                return Some(id);
            }
        }
        None
    }
}

impl Session {
    async fn serve(mut self, mut control: Control, server_cancel: CancellationToken) {
        let background = CancellationToken::new();
        self.tasks.spawn(udp::demux(
            Arc::clone(&self.transport),
            Arc::clone(&self.udp),
            background.clone(),
        ));
        self.tasks.spawn(refuse_streams(
            Arc::clone(&self.transport),
            background.clone(),
        ));

        let mut ping = tokio::time::interval(PING_EVERY);
        ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut last_heard = Instant::now();
        let mut nonce = 0u64;
        let mut told_client = false;

        let mine = self.live.cancel.clone();
        let reason = loop {
            tokio::select! {
                () = mine.cancelled() => {
                    let superseded = !server_cancel.is_cancelled();
                    let why = if superseded {
                        "another session for this key took over"
                    } else {
                        "server shutting down"
                    };
                    told_client = control.send(&ServerMessage::Shutdown { reason: why.to_owned() }).await.is_ok();
                    break if superseded { CloseReason::Superseded } else { CloseReason::Shutdown };
                }
                _ = ping.tick() => {
                    if last_heard.elapsed() > PONG_DEADLINE {
                        break CloseReason::Unresponsive;
                    }
                    nonce += 1;
                    if control.send(&ServerMessage::Ping { nonce }).await.is_err() {
                        break CloseReason::ControlClosed;
                    }
                }
                msg = control.next::<ClientMessage>() => {
                    let Some(msg) = msg else { break CloseReason::PeerLeft };
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
                        ClientMessage::Hello { .. } => break CloseReason::DuplicateHello,
                    };
                    if let Some(reply) = reply
                        && control.send(&reply).await.is_err()
                    {
                        break CloseReason::ControlClosed;
                    }
                }
            }
        };

        tracing::info!(reason = reason.as_str(), "session ended");
        let names: Vec<String> = self.services.keys().cloned().collect();
        for name in names {
            self.unbind(&name);
        }
        background.cancel();
        control.finish().await;
        self.tasks.close();
        let _ = tokio::time::timeout(DRAIN, self.tasks.wait()).await;
        retire(&self.shared, self.key, &self.live);
        // A QUIC `close` drops whatever is not yet on the wire, so let a client that was told why
        // read it and close first.
        if told_client {
            let _ = tokio::time::timeout(SHUTDOWN_LINGER, self.transport.closed()).await;
        }
        self.transport.close(reason);
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
            return failed(BindFailure::BadName);
        }
        if self.services.contains_key(service) {
            return failed(BindFailure::InUse);
        }
        let Some(allow) = AllowList::effective(allow, &self.grant.allow) else {
            tracing::warn!(
                service,
                "the service's allow list shares no address with this client's ceiling"
            );
            return failed(BindFailure::NotGranted);
        };
        // The client never learns a UDP visitor's address, so it could not write the header.
        if proxy_protocol && kind == Kind::Udp {
            tracing::warn!(service, "proxy protocol is not supported on udp yet");
            return failed(BindFailure::Unsupported);
        }
        let live = self.services.values().map(|bound| bound.id).collect();
        let Some(id) = self.ids.claim(&live) else {
            tracing::warn!(service, "every service id is taken");
            return failed(BindFailure::InUse);
        };
        let cancel = CancellationToken::new();
        let port = match kind {
            Kind::Tcp => {
                let (port, listener) = match self.open(service, kind, port, net::bind_tcp) {
                    Ok(opened) => opened,
                    Err(reason) => return failed(reason),
                };
                self.tasks.spawn(listener::serve(
                    Arc::clone(&self.transport),
                    listener,
                    listener::Public {
                        service_id: id,
                        port,
                        allow,
                    },
                    Arc::clone(&self.shared),
                    cancel.clone(),
                    self.tasks.clone(),
                ));
                port
            }
            Kind::Udp => {
                let (port, socket) = match self.open(service, kind, port, net::bind_udp) {
                    Ok(opened) => opened,
                    Err(reason) => return failed(reason),
                };
                let bound = Arc::new(UdpService::new(
                    id,
                    service.to_owned(),
                    socket,
                    port,
                    Arc::clone(&self.shared),
                    allow,
                ));
                self.udp
                    .write()
                    .expect("udp services lock")
                    .insert(id, Arc::clone(&bound));
                self.tasks.spawn(udp::serve(
                    bound,
                    Arc::clone(&self.transport),
                    cancel.clone(),
                ));
                port
            }
        };
        self.services
            .insert(service.to_owned(), BoundService { id, port, cancel });
        tracing::info!(service, %port, bind = %self.grant.bind, "bound");
        ServerMessage::Bound {
            service: service.to_owned(),
            service_id: id,
            port: port.number,
            address: self.grant.bind,
        }
    }

    /// A dynamic request walks on past pool ports the host refuses; a fixed one is that port or
    /// nothing. Refused ports stay claimed for the length of the walk, so an exhausted pool ends it.
    fn open<T>(
        &self,
        service: &str,
        kind: Kind,
        fixed: Option<u16>,
        bind: impl Fn(IpAddr, u16) -> io::Result<T>,
    ) -> Result<(Port, T), BindFailure> {
        let mut refused = Vec::new();
        let outcome = loop {
            let claimed = {
                let mut ports = self.shared.ports.lock().expect("port allocator lock");
                match fixed {
                    Some(number) => {
                        let port = Port { number, kind };
                        if !self.grant.allows(port) {
                            break Err(BindFailure::NotGranted);
                        }
                        ports.claim(port).map(|()| port)
                    }
                    None => ports.claim_dynamic(kind).ok_or(BindFailure::InUse),
                }
            };
            let port = match claimed {
                Ok(port) => port,
                Err(reason) => break Err(reason),
            };
            match bind(self.grant.bind, port.number) {
                Ok(socket) => break Ok((port, socket)),
                Err(err) => {
                    tracing::warn!(service, %port, %err, "cannot bind");
                    refused.push(port);
                    if fixed.is_some() {
                        break Err(BindFailure::InUse);
                    }
                }
            }
        };
        for port in refused {
            self.release(port);
        }
        outcome
    }

    /// Only stops the service: a TCP listener task releases its port once its socket is gone, and
    /// a UDP service does the same when its last handle drops.
    fn unbind(&mut self, service: &str) {
        if let Some(bound) = self.services.remove(service) {
            self.udp
                .write()
                .expect("udp services lock")
                .remove(&bound.id);
            bound.cancel.cancel();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_skip_the_ones_a_live_service_holds() {
        let mut ids = ServiceIds::new();
        let live = HashSet::from([1, 2, 4]);
        assert_eq!(ids.claim(&live), Some(3));
        assert_eq!(ids.claim(&live), Some(5));
    }

    #[test]
    fn ids_wrap_past_the_top_onto_a_free_one() {
        let mut ids = ServiceIds { next: u16::MAX };
        let live = HashSet::from([1]);
        assert_eq!(ids.claim(&live), Some(u16::MAX));
        assert_eq!(ids.claim(&live), Some(2));
    }

    #[test]
    fn ids_run_out_when_every_one_is_live() {
        let mut ids = ServiceIds::new();
        let live: HashSet<u16> = (1..=u16::MAX).collect();
        assert_eq!(ids.claim(&live), None);
    }
}
