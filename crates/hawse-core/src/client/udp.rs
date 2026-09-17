use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use bytes::Bytes;
use hawse_proto::msg::{DatagramHeader, reset};
use hawse_proto::packet;
use tokio::net::UdpSocket;
use tokio::sync::{Notify, mpsc};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::visitor::refuse;
use crate::error::chain;
use crate::frame::read_body;
use crate::transport::{RecvHalf, SendHalf, Transport};
use crate::udp::{DropCounts, Drops, FINISH_WAIT, MAX_PAYLOAD, Sender, drain, report_drops};

/// The server's cap does not bound this table: it evicts at its own without telling us.
pub const SESSION_CAP: usize = 4096;
pub const BOUND_WAIT: Duration = Duration::from_secs(5);

/// The UDP services of one connection, by the id the server gave each.
#[derive(Default)]
pub struct Registry {
    services: RwLock<HashMap<u16, Arc<UdpLocal>>>,
    changed: Notify,
}

impl Registry {
    pub fn insert(&self, local: Arc<UdpLocal>) {
        self.services
            .write()
            .expect("udp registry lock")
            .insert(local.id, local);
        self.changed.notify_waiters();
    }

    pub fn get(&self, id: u16) -> Option<Arc<UdpLocal>> {
        self.services
            .read()
            .expect("udp registry lock")
            .get(&id)
            .cloned()
    }

    pub fn clear(&self) {
        self.services.write().expect("udp registry lock").clear();
    }

    /// `Bulk` and `Bound` travel on different streams, so on QUIC either can arrive first.
    pub async fn wait_for(&self, id: u16, deadline: Duration) -> Option<Arc<UdpLocal>> {
        let found = async {
            loop {
                // Created before the lookup: a `Notified` sees every `notify_waiters` from its
                // creation on, so an insert between the lookup and the await is not missed.
                let changed = self.changed.notified();
                if let Some(local) = self.get(id) {
                    return local;
                }
                changed.await;
            }
        };
        tokio::time::timeout(deadline, found).await.ok()
    }
}

pub struct UdpLocal {
    service: String,
    id: u16,
    local: String,
    idle: Duration,
    cap: usize,
    sender: Sender,
    /// Taken by the one bulk stream the server opens for this service.
    queue: Mutex<Option<mpsc::Receiver<Bytes>>>,
    sessions: Mutex<HashMap<u32, Arc<LocalSession>>>,
    /// One buffer for every reader: a socket per session with 64 KiB of its own would be 256 MiB
    /// at the cap, and a shorter buffer would truncate a 65507-byte reply.
    buf: Mutex<Vec<u8>>,
    /// Whoever sends to the public port opens sessions, so trouble with `local` is warned about
    /// once per service and logged at `debug` after, until a reply clears it.
    warned: AtomicBool,
    drops: Arc<Drops>,
    tasks: TaskTracker,
    cancel: CancellationToken,
}

struct LocalSession {
    socket: UdpSocket,
    used: Mutex<Instant>,
    /// A child of the service's, so eviction ends one reader and unbinding ends them all.
    cancel: CancellationToken,
}

impl LocalSession {
    fn touch(&self) {
        *self.used.lock().expect("session clock lock") = Instant::now();
    }

    fn used(&self) -> Instant {
        *self.used.lock().expect("session clock lock")
    }
}

impl UdpLocal {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        service: String,
        id: u16,
        local: String,
        idle: Duration,
        cap: usize,
        transport: Arc<dyn Transport>,
        tasks: TaskTracker,
        cancel: CancellationToken,
    ) -> Arc<Self> {
        let drops = Arc::new(Drops::default());
        let (sender, queue) = Sender::new(transport, Arc::clone(&drops));
        Arc::new(Self {
            service,
            id,
            local,
            idle,
            cap,
            sender,
            queue: Mutex::new(Some(queue)),
            sessions: Mutex::new(HashMap::new()),
            buf: Mutex::new(vec![0u8; MAX_PAYLOAD]),
            warned: AtomicBool::new(false),
            drops,
            tasks,
            cancel,
        })
    }

    /// A packet from the server, off either path, on its way to `local`.
    pub async fn deliver(self: &Arc<Self>, packet: &[u8]) {
        let Ok((header, payload)) = packet::decode(packet) else {
            self.drops.unknown();
            return;
        };
        if header.service_id != self.id {
            self.drops.unknown();
            return;
        }
        let Some(session) = self.session(header.session).await else {
            return;
        };
        session.touch();
        if let Err(err) = session.socket.send(payload).await {
            self.note(&err);
            self.drops.socket();
        }
    }

    fn note(&self, err: &io::Error) {
        let refused = err.kind() == io::ErrorKind::ConnectionRefused;
        if refused && !self.warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                service = self.service,
                local = self.local,
                "local service refused a datagram"
            );
        } else {
            tracing::debug!(service = self.service, local = self.local, err = %chain(err), "local socket error");
        }
    }

    fn no_socket(&self, err: &io::Error) {
        if self.warned.swap(true, Ordering::Relaxed) {
            tracing::debug!(service = self.service, local = self.local, err = %chain(err), "cannot open a socket toward the local service");
        } else {
            tracing::warn!(service = self.service, local = self.local, err = %chain(err), "cannot open a socket toward the local service");
        }
    }

    /// Read before the store, so the common path leaves a line shared by every reader alone.
    fn heard_from_local(&self) {
        if self.warned.load(Ordering::Relaxed) {
            self.warned.store(false, Ordering::Relaxed);
        }
    }

    async fn session(self: &Arc<Self>, id: u32) -> Option<Arc<LocalSession>> {
        if let Some(found) = self.sessions.lock().expect("udp sessions lock").get(&id) {
            return Some(Arc::clone(found));
        }
        let socket = match connect(&self.local).await {
            Ok(socket) => socket,
            Err(err) => {
                self.no_socket(&err);
                self.drops.socket();
                return None;
            }
        };
        let fresh = Arc::new(LocalSession {
            socket,
            used: Mutex::new(Instant::now()),
            cancel: self.cancel.child_token(),
        });
        let session = {
            let mut sessions = self.sessions.lock().expect("udp sessions lock");
            // Evicting under the lock that inserts is what makes the cap exact.
            let full = sessions.len() >= self.cap && !sessions.contains_key(&id);
            let oldest = full.then(|| quietest(&sessions)).flatten();
            if let Some(gone) = oldest.and_then(|id| sessions.remove(&id)) {
                gone.cancel.cancel();
                self.drops.at_cap();
            }
            // The datagram path and the bulk stream deliver from different tasks, so another one
            // may have opened this session while the socket above was being connected.
            Arc::clone(sessions.entry(id).or_insert_with(|| Arc::clone(&fresh)))
        };
        if Arc::ptr_eq(&session, &fresh) {
            tracing::debug!(service = self.service, session = id, "udp session opened");
            self.tasks
                .spawn(read_replies(Arc::clone(self), id, Arc::clone(&session)));
        }
        Some(session)
    }
}

/// A linear scan: it runs only at the cap, once per new session.
fn quietest(sessions: &HashMap<u32, Arc<LocalSession>>) -> Option<u32> {
    sessions
        .iter()
        .min_by_key(|(_, session)| session.used())
        .map(|(&id, _)| id)
}

async fn connect(local: &str) -> io::Result<UdpSocket> {
    let addr = tokio::net::lookup_host(local)
        .await?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no address"))?;
    let from: SocketAddr = if addr.is_ipv4() {
        (Ipv4Addr::UNSPECIFIED, 0).into()
    } else {
        (Ipv6Addr::UNSPECIFIED, 0).into()
    };
    let socket = UdpSocket::bind(from).await?;
    socket.connect(addr).await?;
    Ok(socket)
}

async fn read_replies(local: Arc<UdpLocal>, id: u32, session: Arc<LocalSession>) {
    let header = DatagramHeader {
        service_id: local.id,
        session: id,
    };
    loop {
        let expires = session.used() + local.idle;
        tokio::select! {
            () = session.cancel.cancelled() => break,
            () = tokio::time::sleep_until(expires) => {
                // `deliver` may have used the session since `expires` was read.
                if session.used() + local.idle <= Instant::now() {
                    break;
                }
            }
            ready = session.socket.readable() => {
                let received = ready.and_then(|()| {
                    let mut buf = local.buf.lock().expect("udp buffer lock");
                    let len = session.socket.try_recv(&mut buf)?;
                    session.touch();
                    local.heard_from_local();
                    // Synchronous, so the shared buffer is never held across an await.
                    local.sender.send(header, &buf[..len]);
                    Ok(())
                });
                if let Err(err) = received {
                    // `readable` wakes on readiness the kernel may withdraw before `try_recv`.
                    if err.kind() == io::ErrorKind::WouldBlock {
                        continue;
                    }
                    local.note(&err);
                    // A service that is not up yet answers every datagram this way; closing the
                    // session for it would open a new socket per packet.
                    if err.kind() != io::ErrorKind::ConnectionRefused {
                        break;
                    }
                }
            }
        }
    }
    let mut sessions = local.sessions.lock().expect("udp sessions lock");
    if sessions
        .get(&id)
        .is_some_and(|held| Arc::ptr_eq(held, &session))
    {
        sessions.remove(&id);
    }
}

pub async fn serve_bulk(
    service_id: u16,
    mut send: SendHalf,
    mut recv: RecvHalf,
    registry: Arc<Registry>,
) {
    let Some(local) = registry.wait_for(service_id, BOUND_WAIT).await else {
        tracing::warn!(service_id, "bulk stream for a service we never bound");
        refuse(&mut send, &mut recv, reset::UNKNOWN_SERVICE);
        return;
    };
    let queue = local.queue.lock().expect("bulk queue lock").take();
    let Some(mut queue) = queue else {
        tracing::warn!(
            service = local.service,
            "a second bulk stream for one service"
        );
        refuse(&mut send, &mut recv, reset::UNKNOWN_SERVICE);
        return;
    };
    // One `select!`, never re-entered: `read_body` is two `read_exact`s, and dropping it between
    // them would lose the stream's framing.
    let ended = tokio::select! {
        () = local.cancel.cancelled() => None,
        drained = drain(&mut queue, &mut send) => drained.err(),
        err = async {
            loop {
                match read_body(&mut recv).await {
                    Ok(packet) => local.deliver(&packet).await,
                    Err(err) => break err,
                }
            }
        } => Some(err),
        () = report_drops(&local.service, &local.drops) => None,
    };
    // The server finishing the stream at unbind reads as an error here too, so not a warning.
    if let Some(err) = ended {
        tracing::debug!(service = local.service, err = %chain(&err), "bulk stream ended");
    }
    let counts = local.drops.snapshot();
    if counts != DropCounts::default() {
        tracing::info!(service = local.service, ?counts, "udp packets dropped");
    }
    let _ = tokio::time::timeout(FINISH_WAIT, send.finish()).await;
}

/// Ends with the transport; on the TCP transport, which has no datagrams, that is at once.
pub async fn demux(transport: Arc<dyn Transport>, registry: Arc<Registry>) {
    while let Ok(packet) = transport.recv_datagram().await {
        let Ok((header, _)) = packet::decode(&packet) else {
            continue;
        };
        if let Some(local) = registry.get(header.service_id) {
            local.deliver(&packet).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    use bytes::Bytes;
    use futures_util::future::BoxFuture;
    use hawse_proto::key::PublicKey;

    use crate::transport::{CloseReason, TransportError};
    use crate::udp::IDLE;

    /// Carries nothing, so every reply lands on the bulk queue, where a test can read it.
    struct NoTransport;

    impl Transport for NoTransport {
        fn open_bi(&self) -> BoxFuture<'_, Result<(SendHalf, RecvHalf), TransportError>> {
            Box::pin(async { Err(TransportError::NoDatagrams) })
        }
        fn accept_bi(&self) -> BoxFuture<'_, Result<(SendHalf, RecvHalf), TransportError>> {
            Box::pin(async { Err(TransportError::NoDatagrams) })
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
        fn close(&self, _reason: CloseReason) {}
        fn closed(&self) -> BoxFuture<'_, ()> {
            Box::pin(std::future::pending())
        }
        fn remote_address(&self) -> SocketAddr {
            SocketAddr::from(([127, 0, 0, 1], 1))
        }
        fn peer_key(&self) -> Option<PublicKey> {
            None
        }
    }

    const ID: u16 = 4;

    fn service(local: SocketAddr, idle: Duration) -> (Arc<UdpLocal>, mpsc::Receiver<Bytes>) {
        capped(local, idle, SESSION_CAP)
    }

    fn capped(
        local: SocketAddr,
        idle: Duration,
        cap: usize,
    ) -> (Arc<UdpLocal>, mpsc::Receiver<Bytes>) {
        let service = UdpLocal::new(
            "svc".to_owned(),
            ID,
            local.to_string(),
            idle,
            cap,
            Arc::new(NoTransport),
            TaskTracker::new(),
            CancellationToken::new(),
        );
        let replies = service.queue.lock().unwrap().take().unwrap();
        (service, replies)
    }

    fn packet_for(session: u32, payload: &[u8]) -> Bytes {
        packet::encode(
            DatagramHeader {
                service_id: ID,
                session,
            },
            payload,
        )
        .unwrap()
    }

    /// Answers every datagram with the port it came from, as text.
    async fn port_teller() -> SocketAddr {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 64];
            while let Ok((_, peer)) = socket.recv_from(&mut buf).await {
                let _ = socket
                    .send_to(peer.port().to_string().as_bytes(), peer)
                    .await;
            }
        });
        addr
    }

    async fn reply(replies: &mut mpsc::Receiver<Bytes>) -> (DatagramHeader, String) {
        let packet = tokio::time::timeout(Duration::from_secs(2), replies.recv())
            .await
            .expect("a reply within 2 s")
            .unwrap();
        let (header, payload) = packet::decode(&packet).unwrap();
        (header, String::from_utf8(payload.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn a_reply_comes_back_under_the_session_that_asked() {
        let (service, mut replies) = service(port_teller().await, IDLE);
        service.deliver(&packet_for(5, b"?")).await;
        service.deliver(&packet_for(6, b"?")).await;

        let (first, first_port) = reply(&mut replies).await;
        let (second, second_port) = reply(&mut replies).await;

        let mut sessions = [first.session, second.session];
        sessions.sort_unstable();
        assert_eq!(sessions, [5, 6]);
        assert_ne!(
            first_port, second_port,
            "two sessions shared a local socket"
        );
    }

    #[tokio::test]
    async fn an_idle_session_closes_and_the_next_packet_opens_a_new_socket() {
        let (service, mut replies) = service(port_teller().await, Duration::from_millis(50));
        service.deliver(&packet_for(5, b"?")).await;
        let (_, before) = reply(&mut replies).await;

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(service.sessions.lock().unwrap().is_empty());

        service.deliver(&packet_for(5, b"?")).await;
        let (_, after) = reply(&mut replies).await;
        assert_ne!(before, after);
    }

    #[tokio::test]
    async fn a_local_service_that_is_not_listening_keeps_its_session() {
        let closed = UdpSocket::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap();
        let (service, _replies) = service(closed, IDLE);

        for _ in 0..3 {
            service.deliver(&packet_for(5, b"?")).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        assert_eq!(service.sessions.lock().unwrap().len(), 1);
    }

    /// The flag is the service's, not the session's, so a stranger opening sessions cannot
    /// multiply the warning it gates.
    #[tokio::test]
    async fn a_refusal_raises_the_services_warned_flag() {
        let closed = UdpSocket::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap();
        let (service, _replies) = service(closed, IDLE);

        service.deliver(&packet_for(5, b"?")).await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(service.warned.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn a_reply_lowers_the_services_warned_flag() {
        let (service, mut replies) = service(port_teller().await, IDLE);
        service.warned.store(true, Ordering::Relaxed);

        service.deliver(&packet_for(5, b"?")).await;
        reply(&mut replies).await;

        assert!(!service.warned.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn at_the_cap_the_session_quiet_longest_makes_room_for_a_new_one() {
        let (service, mut replies) = capped(port_teller().await, IDLE, 2);
        for session in 1..=2 {
            service.deliver(&packet_for(session, b"?")).await;
            reply(&mut replies).await;
        }

        service.deliver(&packet_for(3, b"?")).await;

        let (newest, _) = reply(&mut replies).await;
        assert_eq!(newest.session, 3);
        assert_eq!(service.drops.snapshot().at_cap, 1);
        let sessions = service.sessions.lock().unwrap();
        assert_eq!(sessions.len(), 2);
        assert!(!sessions.contains_key(&1));
    }

    #[tokio::test]
    async fn a_packet_for_another_service_is_counted_and_opens_nothing() {
        let (service, _replies) = service(port_teller().await, IDLE);
        let stray = packet::encode(
            DatagramHeader {
                service_id: ID + 1,
                session: 5,
            },
            b"?",
        )
        .unwrap();

        service.deliver(&stray).await;

        assert_eq!(service.drops.snapshot().unknown, 1);
        assert!(service.sessions.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn waiting_for_a_service_ends_when_it_registers() {
        let registry = Arc::new(Registry::default());
        let waiter = tokio::spawn({
            let registry = Arc::clone(&registry);
            async move {
                registry
                    .wait_for(ID, Duration::from_secs(5))
                    .await
                    .is_some()
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        registry.insert(service(port_teller().await, IDLE).0);
        assert!(waiter.await.unwrap());
    }

    #[tokio::test]
    async fn waiting_for_a_service_that_never_registers_gives_up() {
        let registry = Registry::default();
        assert!(
            registry
                .wait_for(ID, Duration::from_millis(50))
                .await
                .is_none()
        );
    }
}
