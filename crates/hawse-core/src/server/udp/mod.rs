pub mod table;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use hawse_proto::msg::{DatagramHeader, StreamOpen};
use hawse_proto::packet;
use hawse_proto::port::Port;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use self::table::SessionTable;
use super::Shared;
use crate::error::chain;
use crate::frame::{read_body, write_frame};
use crate::transport::Transport;
use crate::udp::{Drops, FINISH_WAIT, IDLE, MAX_PAYLOAD, Sender, drain, report_drops};

const SWEEP_EVERY: Duration = Duration::from_secs(10);

pub type UdpServices = Arc<RwLock<HashMap<u16, Arc<UdpService>>>>;

pub struct UdpService {
    id: u16,
    name: String,
    socket: UdpSocket,
    table: Mutex<SessionTable>,
    drops: Arc<Drops>,
    // After `socket`: fields drop in declaration order, and a port released while its socket is
    // still open could be claimed by the next bind and refused by the kernel.
    _claim: PortClaim,
}

struct PortClaim {
    port: Port,
    shared: Arc<Shared>,
}

impl Drop for PortClaim {
    fn drop(&mut self) {
        self.shared
            .ports
            .lock()
            .expect("port allocator lock")
            .release(self.port);
    }
}

impl UdpService {
    /// Takes over `port`'s claim on the allocator and releases it once the socket is closed.
    pub fn new(id: u16, name: String, socket: UdpSocket, port: Port, shared: Arc<Shared>) -> Self {
        Self {
            id,
            name,
            socket,
            table: Mutex::new(SessionTable::new(shared.udp_sessions, IDLE)),
            drops: Arc::default(),
            _claim: PortClaim { port, shared },
        }
    }

    /// A packet from the client, off either path, on its way to the visitor whose session it names.
    pub async fn deliver(&self, packet: &[u8]) {
        let Ok((header, payload)) = packet::decode(packet) else {
            self.drops.unknown();
            return;
        };
        if header.service_id != self.id {
            self.drops.unknown();
            return;
        }
        let visitor = self
            .table
            .lock()
            .expect("session table lock")
            .outbound(header.session, Instant::now());
        let Some(visitor) = visitor else {
            self.drops.unknown();
            return;
        };
        if let Err(err) = self.socket.send_to(payload, visitor).await {
            tracing::debug!(service = self.name, %visitor, err = %chain(&err), "send failed");
            self.drops.socket();
        }
    }
}

/// Runs until `cancel`; the service's port is released when the last handle to it drops.
pub async fn serve(
    service: Arc<UdpService>,
    transport: Arc<dyn Transport>,
    cancel: CancellationToken,
) {
    let (sender, queue) = Sender::new(Arc::clone(&transport), Arc::clone(&service.drops));
    tokio::join!(
        inbound(&service, &sender, &cancel),
        bulk(&service, &*transport, queue, &cancel),
    );
    tracing::info!(
        service = service.name,
        sent = ?sender.sent(),
        drops = ?service.drops.snapshot(),
        "udp service ended"
    );
}

async fn inbound(service: &UdpService, sender: &Sender, cancel: &CancellationToken) {
    let mut buf = vec![0u8; MAX_PAYLOAD];
    let mut sweep = tokio::time::interval(SWEEP_EVERY);
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            _ = sweep.tick() => {
                service.table.lock().expect("session table lock").sweep(Instant::now());
            }
            received = service.socket.recv_from(&mut buf) => {
                let (len, visitor) = match received {
                    Ok(received) => received,
                    Err(err) => {
                        tracing::debug!(service = service.name, err = %chain(&err), "receive failed");
                        continue;
                    }
                };
                let seen = service
                    .table
                    .lock()
                    .expect("session table lock")
                    .inbound(visitor, Instant::now());
                if seen.evicted {
                    service.drops.at_cap();
                }
                if seen.fresh {
                    tracing::debug!(service = service.name, %visitor, session = seen.session, "udp session opened");
                }
                let header = DatagramHeader { service_id: service.id, session: seen.session };
                sender.send(header, &buf[..len]);
            }
        }
    }
}

async fn bulk(
    service: &UdpService,
    transport: &dyn Transport,
    mut queue: mpsc::Receiver<Bytes>,
    cancel: &CancellationToken,
) {
    let opened = tokio::select! {
        () = cancel.cancelled() => return,
        opened = transport.open_bi() => opened,
    };
    let (mut send, mut recv) = match opened {
        Ok(halves) => halves,
        Err(err) => {
            tracing::warn!(service = service.name, err = %chain(&err), "cannot open the bulk stream");
            return;
        }
    };
    let open = StreamOpen::Bulk {
        service_id: service.id,
    };
    let wrote = tokio::select! {
        () = cancel.cancelled() => return,
        wrote = write_frame(&mut send, &open) => wrote,
    };
    if let Err(err) = wrote {
        tracing::warn!(service = service.name, err = %chain(&err), "cannot open the bulk stream");
        return;
    }
    // One `select!`, never re-entered: `read_body` is two `read_exact`s, and dropping it between
    // them would lose the stream's framing.
    let ended = tokio::select! {
        () = cancel.cancelled() => None,
        drained = drain(&mut queue, &mut send) => drained.err(),
        err = async {
            loop {
                match read_body(&mut recv).await {
                    Ok(packet) => service.deliver(&packet).await,
                    Err(err) => break err,
                }
            }
        } => Some(err),
        () = report_drops(&service.name, &service.drops) => None,
    };
    // An ordinary client disconnect reaches this read before the cancel does, so not a warning.
    if let Some(err) = ended {
        tracing::debug!(
            service = service.name,
            err = %chain(&err),
            "bulk stream ended; payloads too large for a datagram will drop"
        );
    }
    // On the TCP transport a finish waits on the stream's command channel, which a stalled link
    // fills, and this task holds the service's port until it returns.
    let _ = tokio::time::timeout(FINISH_WAIT, send.finish()).await;
}

/// Datagrams from the client, routed to the service each names. Ends with `cancel` or the
/// transport; on the TCP transport, which has no datagrams, that is at once.
pub async fn demux(
    transport: Arc<dyn Transport>,
    services: UdpServices,
    cancel: CancellationToken,
) {
    loop {
        let packet = tokio::select! {
            () = cancel.cancelled() => return,
            packet = transport.recv_datagram() => match packet {
                Ok(packet) => packet,
                Err(_) => return,
            },
        };
        let Ok((header, _)) = packet::decode(&packet) else {
            continue;
        };
        let service = services
            .read()
            .expect("udp services lock")
            .get(&header.service_id)
            .cloned();
        if let Some(service) = service {
            service.deliver(&packet).await;
        }
    }
}
