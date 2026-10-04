use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use hawse_proto::key::PublicKey;
use hawse_proto::msg::{BindFailure, ClientMessage, DenyReason, ServerMessage, reset};
use hawse_proto::name;
use hawse_proto::port::{Kind, Port};
use ipnet::IpNet;
use tokio::sync::Semaphore;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::{CancellationToken, DropGuard};
use tokio_util::task::TaskTracker;
use tracing::Instrument as _;

use super::policy::Grant;
use super::udp::{self, UdpService, UdpServices};
use super::{AGENT, Live, Shared, listener};
use crate::allow::AllowList;
use crate::control::Control;
use crate::error::chain;
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
    let Ok(Ok((mut control, hello))) = greeted else {
        if stranger {
            shared.auth_failed(remote.ip());
        }
        transport.close(CloseReason::NoHello);
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
        let (claim, previous) = Claim::stake(Arc::clone(&shared), key, live);
        if let Some(previous) = previous {
            previous.cancel.cancel();
            tracing::info!(previous = %previous.remote, "superseding this key's previous session");
            // Not welcomed yet, so a shutdown finds nothing here to tell the client or to drain.
            tokio::select! {
                () = cancel.cancelled() => {
                    transport.close(CloseReason::Shutdown);
                    return;
                }
                _ = tokio::time::timeout(SUPERSEDE_WAIT, previous.done.cancelled()) => {}
            }
        }
        let welcome = ServerMessage::Welcome {
            agent: AGENT.to_owned(),
            client_name: grant.name.clone(),
        };
        if control.send(&welcome).await.is_err() {
            transport.close(CloseReason::ControlClosed);
            return;
        }
        tracing::info!(%agent, "client connected");
        let session = Session {
            transport,
            streams: Arc::new(Semaphore::new(shared.streams)),
            shared,
            grant,
            services: HashMap::new(),
            ids: ServiceIds::new(),
            tasks: TaskTracker::new(),
            udp: Arc::default(),
            claim,
        };
        session.serve(control, cancel).await;
    }
    .instrument(span)
    .await;
}

/// A session's hold on its key. Dropping it signals that the session is done with its ports, then
/// gives the key up unless a newer session has already taken it. On the way out of `serve` the
/// ports are free by then; on an unwind their listeners have only been told to stop, and release
/// them a moment later. A drop and not a call, so a session that panics does not leave every later
/// one for its key waiting out `SUPERSEDE_WAIT`.
struct Claim {
    shared: Arc<Shared>,
    key: PublicKey,
    live: Arc<Live>,
}

impl Claim {
    /// Also returns the session that held the key until now, if there was one.
    fn stake(shared: Arc<Shared>, key: PublicKey, live: Arc<Live>) -> (Self, Option<Arc<Live>>) {
        let previous = shared
            .sessions
            .lock()
            .expect("sessions lock")
            .insert(key, Arc::clone(&live));
        (Self { shared, key, live }, previous)
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.live.done.cancel();
        // This runs during an unwind as well, where a second panic would abort the process.
        let Ok(mut sessions) = self.shared.sessions.lock() else {
            return;
        };
        if sessions
            .get(&self.key)
            .is_some_and(|held| Arc::ptr_eq(held, &self.live))
        {
            sessions.remove(&self.key);
        }
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
    /// One permit per stream this session has open toward its client. Past its limit yamux tears
    /// the connection down and QUIC parks the open, each parked one holding a visitor's socket, so
    /// the count is kept here, where a visitor can be turned away instead.
    streams: Arc<Semaphore>,
    shared: Arc<Shared>,
    grant: Grant,
    services: HashMap<String, BoundService>,
    ids: ServiceIds,
    tasks: TaskTracker,
    udp: UdpServices,
    // After `services`: fields drop in declaration order, and on an unwind `done` must not fire
    // before each service has been told to stop.
    claim: Claim,
}

struct BoundService {
    id: u16,
    port: Port,
    /// Stops the service when it drops, so a session that unwinds stops what it bound as well.
    cancel: DropGuard,
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
        // A guard and not only the call further down, which an unwind never reaches: the demux
        // holds every UDP service, and a service's port stays taken for as long as it is held. A
        // local drops before `self` does, so this too is ahead of `done`.
        let background = background.drop_guard();

        let mut ping = tokio::time::interval(PING_EVERY);
        ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut last_heard = Instant::now();
        let mut nonce = 0u64;
        let mut told = None;

        let mine = self.claim.live.cancel.clone();
        let reason = loop {
            tokio::select! {
                () = mine.cancelled() => {
                    let superseded = !server_cancel.is_cancelled();
                    let why = if superseded {
                        "another session for this key took over"
                    } else {
                        "server shutting down"
                    };
                    let shutdown = ServerMessage::Shutdown { reason: why.to_owned() };
                    told = control.send(&shutdown).await.is_ok().then(tokio::time::Instant::now);
                    break if superseded { CloseReason::Superseded } else { CloseReason::Shutdown };
                }
                () = tokio::time::sleep_until(last_heard + PONG_DEADLINE) => {
                    break CloseReason::Unresponsive;
                }
                _ = ping.tick() => {
                    nonce += 1;
                    if control.send(&ServerMessage::Ping { nonce }).await.is_err() {
                        break CloseReason::ControlClosed;
                    }
                }
                msg = control.try_next::<ClientMessage>() => {
                    let msg = match msg {
                        Ok(Some(msg)) => msg,
                        Ok(None) => break CloseReason::PeerLeft,
                        Err(err) => {
                            tracing::warn!(err = %chain(&err), "malformed control frame");
                            break CloseReason::ControlClosed;
                        }
                    };
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
        drop(background);
        control.finish().await;
        self.tasks.close();
        let _ = tokio::time::timeout(DRAIN, self.tasks.wait()).await;
        drop(self.claim);
        // A QUIC `close` drops whatever is not yet on the wire, so let a client that was told why
        // read it and close first. Counted from the telling, so it runs alongside the drain and
        // the two together stay inside the server's wait.
        if let Some(told) = told {
            let _ = tokio::time::timeout_at(told + SHUTDOWN_LINGER, self.transport.closed()).await;
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
                    Arc::clone(&self.streams),
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
                    proxy_protocol,
                ));
                self.udp
                    .write()
                    .expect("udp services lock")
                    .insert(id, Arc::clone(&bound));
                self.tasks.spawn(udp::serve(
                    bound,
                    Arc::clone(&self.transport),
                    Arc::clone(&self.streams),
                    cancel.clone(),
                ));
                port
            }
        };
        self.services.insert(
            service.to_owned(),
            BoundService {
                id,
                port,
                cancel: cancel.drop_guard(),
            },
        );
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
                    tracing::warn!(service, %port, err = %chain(&err), "cannot bind");
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
            drop(bound.cancel);
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
    use std::future::Future as _;
    use std::net::Ipv4Addr;
    use std::pin::pin;
    use std::sync::OnceLock;
    use std::task::{Context, Wake, Waker};

    use bytes::Bytes;
    use futures_util::future::BoxFuture;

    use super::*;
    use crate::identity::Identity;
    use crate::tls;
    use crate::transport::quic::{self, QuicTransport, Tuning};
    use crate::transport::{RecvHalf, SendHalf};

    /// The client's end of a session built by `unserved`.
    struct Peer {
        control: Control,
        conn: quinn::Connection,
        _endpoints: (quinn::Endpoint, quinn::Endpoint),
    }

    impl Peer {
        async fn ask(&mut self, service: &str, kind: Kind, port: Option<u16>) {
            let bind = ClientMessage::Bind {
                service: service.to_owned(),
                kind,
                port,
                allow: vec![],
                proxy_protocol: false,
            };
            self.control.send(&bind).await.unwrap();
        }

        /// The answer to a bind; the keepalives around it are skipped.
        async fn bind(&mut self, service: &str, kind: Kind, port: Option<u16>) -> ServerMessage {
            self.ask(service, kind, port).await;
            loop {
                let reply = self.control.next::<ServerMessage>().await;
                match reply.expect("control stream open") {
                    ServerMessage::Ping { .. } | ServerMessage::Pong { .. } => {}
                    reply => return reply,
                }
            }
        }
    }

    /// A session over a real connection, not yet served, with its control stream and its client.
    /// Built by hand and not by `run`, so a test can reach the session's own state.
    async fn unserved(shared: &Arc<Shared>) -> (Session, Control, Peer) {
        let server_id = Identity::generate().unwrap();
        let client_id = Identity::generate().unwrap();
        let (cert, key) = server_id.certificate().unwrap();
        let server = quic::listen(
            "127.0.0.1:0".parse().unwrap(),
            tls::server_config(cert, key, tls::provider()).unwrap(),
            Tuning::SERVER,
        )
        .unwrap();
        let addr = server.local_addr().unwrap();
        let (cert, key) = client_id.certificate().unwrap();
        let client = quic::dialer(
            tls::client_config(cert, key, server_id.public_key(), tls::provider()).unwrap(),
            Tuning::CLIENT,
            addr,
        )
        .unwrap();
        let (accepted, conn) = tokio::join!(
            async { server.accept().await.unwrap().await.unwrap() },
            async { quic::connect(&client, addr).await.unwrap() },
        );
        let transport: Arc<dyn Transport> = Arc::new(QuicTransport(accepted));

        let (send, recv) = conn.open_bi().await.unwrap();
        let mut peer = Peer {
            control: Control::new(SendHalf::Quic(send), RecvHalf::Quic(recv)),
            conn,
            _endpoints: (server, client),
        };
        // A stream reaches the other end with its first bytes, not before.
        let ping = ClientMessage::Ping { nonce: 0 };
        peer.control.send(&ping).await.unwrap();
        let (send, recv) = transport.accept_bi().await.unwrap();

        let live = Arc::new(Live {
            cancel: CancellationToken::new(),
            done: CancellationToken::new(),
            remote: "127.0.0.1:1".parse().unwrap(),
        });
        let (claim, _) = Claim::stake(Arc::clone(shared), client_id.public_key(), live);
        let session = Session {
            transport,
            streams: Arc::new(Semaphore::new(shared.streams)),
            shared: Arc::clone(shared),
            grant: Grant {
                name: "test".to_owned(),
                ports: vec![
                    "40000-41000".parse().unwrap(),
                    "40000-41000/udp".parse().unwrap(),
                ],
                bind: Ipv4Addr::LOCALHOST.into(),
                allow: vec![],
            },
            services: HashMap::new(),
            ids: ServiceIds::new(),
            tasks: TaskTracker::new(),
            udp: Arc::default(),
            claim,
        };
        (session, Control::new(send, recv), peer)
    }

    /// Binds `port` again and again until the server grants it, for up to 5 s: a session that
    /// unwound has told its listeners to stop, and they let go of their ports a moment later.
    async fn rebinds(peer: &mut Peer, service: &str, port: Port) -> bool {
        let granted = async {
            loop {
                let reply = peer.bind(service, port.kind, Some(port.number)).await;
                if matches!(reply, ServerMessage::Bound { .. }) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), granted)
            .await
            .is_ok()
    }

    /// Waits for the session to close `peer`'s connection, which has to be for `reason`.
    async fn expect_closed(peer: &Peer, reason: CloseReason) {
        let closed = tokio::time::timeout(Duration::from_secs(5), peer.conn.closed())
            .await
            .expect("the session closes within 5 s");
        assert!(
            matches!(
                &closed,
                quinn::ConnectionError::ApplicationClosed(end)
                    if end.error_code == quinn::VarInt::from_u32(reason.code())
            ),
            "{closed:?}"
        );
    }

    #[tokio::test]
    async fn a_session_that_panics_frees_the_ports_it_bound() {
        let shared = Arc::new(Shared::for_tests());
        let (session, control, mut first) = unserved(&shared).await;
        let services = Arc::clone(&session.udp);
        let serving = tokio::spawn(session.serve(control, CancellationToken::new()));
        let mut bound = Vec::new();
        for (service, kind) in [("web", Kind::Tcp), ("dns", Kind::Udp)] {
            let ServerMessage::Bound { port: number, .. } = first.bind(service, kind, None).await
            else {
                panic!("expected Bound");
            };
            bound.push((service, Port { number, kind }));
        }

        // The session takes this lock for every UDP bind, and finding it poisoned is a panic of
        // its own, in the middle of serving.
        let poisoning = std::panic::catch_unwind(move || {
            let _held = services.write().unwrap();
            panic!("poisoning the lock");
        });
        assert!(poisoning.is_err());
        first.ask("more", Kind::Udp, None).await;
        assert!(serving.await.unwrap_err().is_panic());

        // `first` is still connected, so nothing here is freed by the transport going away.
        let (session, control, mut second) = unserved(&shared).await;
        tokio::spawn(session.serve(control, CancellationToken::new()));
        for (service, port) in bound {
            assert!(
                rebinds(&mut second, service, port).await,
                "{port} is still held"
            );
        }
    }

    /// Notes, at the moment it is woken, whether `stopped` had been cancelled by then.
    struct Watcher {
        stopped: CancellationToken,
        at_wake: OnceLock<bool>,
    }

    impl Wake for Watcher {
        fn wake(self: Arc<Self>) {
            let _ = self.at_wake.set(self.stopped.is_cancelled());
        }
    }

    #[tokio::test]
    async fn a_dropped_session_stops_its_services_before_it_signals_done() {
        let shared = Arc::new(Shared::for_tests());
        let (mut session, _control, _peer) = unserved(&shared).await;
        let live = Arc::clone(&session.claim.live);
        let stopped = CancellationToken::new();
        session.services.insert(
            "web".to_owned(),
            BoundService {
                id: 1,
                port: "40000".parse().unwrap(),
                cancel: stopped.clone().drop_guard(),
            },
        );

        // Cancelling a token wakes its waiters before `cancel` returns, so this sees the service's
        // token as it stood when `done` fired.
        let watcher = Arc::new(Watcher {
            stopped,
            at_wake: OnceLock::new(),
        });
        let waker = Waker::from(Arc::clone(&watcher));
        let mut done = pin!(live.done.cancelled());
        assert!(
            done.as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );

        drop(session);
        assert_eq!(watcher.at_wake.get(), Some(&true));
    }

    #[tokio::test]
    async fn a_session_whose_drain_runs_out_still_ends_inside_the_servers_wait() {
        let shared = Arc::new(Shared::for_tests());
        let (session, control, _peer) = unserved(&shared).await;
        let live = Arc::clone(&session.claim.live);
        // Stands in for a visitor the shutdown does not end, so the drain runs its whole length.
        session.tasks.spawn(std::future::pending::<()>());
        let stop = CancellationToken::new();
        let serving = tokio::spawn(session.serve(control, stop.clone()));

        // `_peer` is told and neither reads it nor closes, so nothing cuts the linger short.
        stop.cancel();
        live.cancel.cancel();
        tokio::time::timeout(crate::server::DRAIN, serving)
            .await
            .expect("the session ends inside the server's wait")
            .unwrap();
    }

    #[tokio::test]
    async fn a_client_that_goes_quiet_is_dropped_at_the_deadline_and_not_a_tick_later() {
        let shared = Arc::new(Shared::for_tests());
        let (session, control, mut peer) = unserved(&shared).await;
        let serving = tokio::spawn(session.serve(control, CancellationToken::new()));
        // Out of step with the ping tick, as a client's last word is: the deadline then falls
        // between two ticks.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let ping = ClientMessage::Ping { nonce: 1 };
        peer.control.send(&ping).await.unwrap();
        loop {
            let reply = peer.control.next::<ServerMessage>().await;
            if let ServerMessage::Pong { nonce: 1 } = reply.expect("control stream open") {
                break;
            }
        }

        // Paused only for the wait: a paused clock runs ahead of whatever is still on a real
        // socket.
        tokio::time::pause();
        let heard = Instant::now();
        tokio::time::timeout(2 * PONG_DEADLINE, serving)
            .await
            .expect("the session gives up on a client it no longer hears from")
            .unwrap();
        let took = heard.elapsed();
        tokio::time::resume();
        // The session heard that ping a moment before the clock stopped, and the tick after the
        // deadline is nearly 15 s past it.
        assert!(
            (Duration::from_secs(44)..Duration::from_secs(46)).contains(&took),
            "{took:?}"
        );
        expect_closed(&peer, CloseReason::Unresponsive).await;
    }

    #[tokio::test]
    async fn a_frame_that_is_no_message_ends_the_session_as_a_closed_control_stream() {
        let shared = Arc::new(Shared::for_tests());
        let (session, control, mut peer) = unserved(&shared).await;
        tokio::spawn(session.serve(control, CancellationToken::new()));
        // A variant number that says more bytes follow, in a frame that has none.
        peer.control.send(&u8::MAX).await.unwrap();
        expect_closed(&peer, CloseReason::ControlClosed).await;
    }

    #[tokio::test]
    async fn a_client_that_finishes_its_control_stream_has_left() {
        let shared = Arc::new(Shared::for_tests());
        let (session, control, mut peer) = unserved(&shared).await;
        tokio::spawn(session.serve(control, CancellationToken::new()));
        peer.control.finish().await;
        expect_closed(&peer, CloseReason::PeerLeft).await;
    }

    /// A connection that is lost before its peer opens a stream. Keeps the reason it was closed
    /// with, the first one as on a real transport.
    struct Lost {
        closed: OnceLock<CloseReason>,
    }

    impl Transport for Lost {
        fn open_bi(&self) -> BoxFuture<'_, Result<(SendHalf, RecvHalf), TransportError>> {
            Box::pin(async { Err(TransportError::Io(io::ErrorKind::NotConnected.into())) })
        }
        fn accept_bi(&self) -> BoxFuture<'_, Result<(SendHalf, RecvHalf), TransportError>> {
            Box::pin(async { Err(TransportError::Io(io::ErrorKind::NotConnected.into())) })
        }
        fn send_datagram(&self, _data: Bytes) -> Result<(), TransportError> {
            Err(TransportError::NoDatagrams)
        }
        fn recv_datagram(&self) -> BoxFuture<'_, Result<Bytes, TransportError>> {
            Box::pin(async { Err(TransportError::NoDatagrams) })
        }
        fn max_datagram_size(&self) -> Option<usize> {
            None
        }
        fn datagram_send_buffer_space(&self) -> usize {
            0
        }
        fn close(&self, reason: CloseReason) {
            let _ = self.closed.set(reason);
        }
        fn closed(&self) -> BoxFuture<'_, ()> {
            Box::pin(std::future::pending())
        }
        fn remote_address(&self) -> SocketAddr {
            SocketAddr::from(([127, 0, 0, 1], 1))
        }
        fn peer_key(&self) -> Option<PublicKey> {
            Some(PublicKey::from_bytes([1; 32]))
        }
    }

    #[tokio::test]
    async fn a_connection_lost_before_its_control_stream_is_closed_for_want_of_a_hello() {
        let lost = Arc::new(Lost {
            closed: OnceLock::new(),
        });
        run(
            lost.clone(),
            lost.remote_address(),
            Arc::new(Shared::for_tests()),
            CancellationToken::new(),
        )
        .await;
        assert_eq!(lost.closed.get(), Some(&CloseReason::NoHello));
    }

    #[tokio::test]
    async fn a_session_that_panics_gives_up_its_key() {
        let shared = Arc::new(Shared::for_tests());
        let key = PublicKey::from_bytes([1; 32]);
        let live = Arc::new(Live {
            cancel: CancellationToken::new(),
            done: CancellationToken::new(),
            remote: "127.0.0.1:1".parse().unwrap(),
        });
        let session = tokio::spawn({
            let (shared, live) = (Arc::clone(&shared), Arc::clone(&live));
            async move {
                let _claim = Claim::stake(shared, key, live);
                panic!("mid-session");
            }
        });
        assert!(session.await.unwrap_err().is_panic());
        assert!(live.done.is_cancelled());
        assert!(shared.sessions.lock().unwrap().is_empty());
    }

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
