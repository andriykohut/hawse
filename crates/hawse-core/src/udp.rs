use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use hawse_proto::msg::DatagramHeader;
use hawse_proto::packet;
use tokio::sync::mpsc;

use crate::frame::{StreamFrameError, write_body};
use crate::transport::{SendHalf, Transport, TransportError};

pub const IDLE: Duration = Duration::from_secs(60);
pub const BULK_QUEUE: usize = 256;
/// The largest datagram a socket can hand us, headers included, so a receive never truncates.
pub const MAX_PAYLOAD: usize = 65535;
pub const FINISH_WAIT: Duration = Duration::from_secs(1);
const REPORT_EVERY: Duration = Duration::from_secs(60);

#[derive(Debug, Default)]
pub struct Drops {
    bulk_full: AtomicU64,
    at_cap: AtomicU64,
    unknown: AtomicU64,
    socket: AtomicU64,
    queue_full: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DropCounts {
    pub bulk_full: u64,
    pub at_cap: u64,
    pub unknown: u64,
    pub socket: u64,
    pub queue_full: u64,
}

impl Drops {
    pub fn bulk_full(&self) {
        self.bulk_full.fetch_add(1, Ordering::Relaxed);
    }

    pub fn at_cap(&self) {
        self.at_cap.fetch_add(1, Ordering::Relaxed);
    }

    pub fn unknown(&self) {
        self.unknown.fetch_add(1, Ordering::Relaxed);
    }

    pub fn socket(&self) {
        self.socket.fetch_add(1, Ordering::Relaxed);
    }

    pub fn queue_full(&self) {
        self.queue_full.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> DropCounts {
        DropCounts {
            bulk_full: self.bulk_full.load(Ordering::Relaxed),
            at_cap: self.at_cap.load(Ordering::Relaxed),
            unknown: self.unknown.load(Ordering::Relaxed),
            socket: self.socket.load(Ordering::Relaxed),
            queue_full: self.queue_full.load(Ordering::Relaxed),
        }
    }
}

/// Never returns; run it as one arm of the `select!` that owns the service's bulk stream.
pub async fn report_drops(service: &str, drops: &Drops) {
    let mut every = tokio::time::interval(REPORT_EVERY);
    let mut reported = DropCounts::default();
    loop {
        every.tick().await;
        let counts = drops.snapshot();
        if counts != reported {
            tracing::debug!(service, ?counts, "udp packets dropped so far");
            reported = counts;
        }
    }
}

#[derive(Debug, Default)]
struct Sent {
    datagram: AtomicU64,
    bulk: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SentCounts {
    pub datagram: u64,
    pub bulk: u64,
}

/// Never waits: a payload that fits goes out as a datagram, anything else is queued for the bulk
/// stream, and a full queue drops it. A UDP sender must not feel the tunnel's back-pressure.
#[derive(Clone)]
pub struct Sender {
    transport: Arc<dyn Transport>,
    bulk: mpsc::Sender<Bytes>,
    drops: Arc<Drops>,
    sent: Arc<Sent>,
}

impl Sender {
    /// The receiver is the bulk stream's outbound queue; hand it to `drain`.
    pub fn new(transport: Arc<dyn Transport>, drops: Arc<Drops>) -> (Self, mpsc::Receiver<Bytes>) {
        let (bulk, queue) = mpsc::channel(BULK_QUEUE);
        (
            Self {
                transport,
                bulk,
                drops,
                sent: Arc::default(),
            },
            queue,
        )
    }

    /// quinn's datagram size starts near 1160 and only grows with MTU discovery, so a payload
    /// under 1200 bytes can still take the bulk stream for the life of a connection.
    pub fn sent(&self) -> SentCounts {
        SentCounts {
            datagram: self.sent.datagram.load(Ordering::Relaxed),
            bulk: self.sent.bulk.load(Ordering::Relaxed),
        }
    }

    pub fn send(&self, header: DatagramHeader, payload: &[u8]) {
        let Ok(packet) = packet::encode(header, payload) else {
            // No socket returns this: UDP caps a payload at 65527 bytes, which fits a frame with its header.
            tracing::debug!(len = payload.len(), "payload does not fit a frame");
            return;
        };
        let fits = self
            .transport
            .max_datagram_size()
            .is_some_and(|max| packet.len() <= max);
        if fits {
            // A full queue makes quinn evict the oldest queued datagram rather than reject this
            // one, which is the right policy for live UDP; this counts those evictions, at least
            // one per bump, since quinn itself does not report them.
            if self.transport.datagram_send_buffer_space() < packet.len() {
                self.drops.queue_full();
            }
            match self.transport.send_datagram(packet.clone()) {
                // The path MTU can shrink between the size check and the send.
                Err(TransportError::DatagramTooLarge | TransportError::NoDatagrams) => {}
                Ok(()) => {
                    self.sent.datagram.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                Err(_) => return,
            }
        }
        if self.bulk.try_send(packet).is_err() {
            self.drops.bulk_full();
        } else {
            self.sent.bulk.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Ends with `Ok` only once every `Sender` is gone.
pub async fn drain(
    queue: &mut mpsc::Receiver<Bytes>,
    send: &mut SendHalf,
) -> Result<(), StreamFrameError> {
    while let Some(packet) = queue.recv().await {
        write_body(send, &packet).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::atomic::AtomicUsize;

    use futures_util::future::BoxFuture;
    use hawse_proto::key::PublicKey;
    use hawse_proto::msg::DatagramHeader;

    use super::*;
    use crate::transport::{CloseReason, RecvHalf};

    struct FakeTransport {
        space: usize,
        sends: AtomicUsize,
    }

    impl Transport for FakeTransport {
        fn open_bi(&self) -> BoxFuture<'_, Result<(SendHalf, RecvHalf), TransportError>> {
            Box::pin(async { Err(TransportError::NoDatagrams) })
        }
        fn accept_bi(&self) -> BoxFuture<'_, Result<(SendHalf, RecvHalf), TransportError>> {
            Box::pin(async { Err(TransportError::NoDatagrams) })
        }
        fn send_datagram(&self, _data: Bytes) -> Result<(), TransportError> {
            self.sends.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        fn recv_datagram(&self) -> BoxFuture<'_, Result<Bytes, TransportError>> {
            Box::pin(async { Err(TransportError::NoDatagrams) })
        }
        fn max_datagram_size(&self) -> Option<usize> {
            Some(usize::MAX)
        }
        fn datagram_send_buffer_space(&self) -> usize {
            self.space
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

    fn header() -> DatagramHeader {
        DatagramHeader {
            service_id: 1,
            session: 1,
        }
    }

    #[test]
    fn a_full_queue_still_sends_but_counts_a_drop() {
        let transport = Arc::new(FakeTransport {
            space: 0,
            sends: AtomicUsize::new(0),
        });
        let drops = Arc::new(Drops::default());
        let (sender, _bulk) = Sender::new(transport.clone(), drops.clone());

        sender.send(header(), b"payload");

        assert_eq!(drops.snapshot().queue_full, 1);
        assert_eq!(transport.sends.load(Ordering::Relaxed), 1);
        assert_eq!(sender.sent().datagram, 1);
    }

    #[test]
    fn ample_space_counts_no_drop() {
        let transport = Arc::new(FakeTransport {
            space: MAX_PAYLOAD,
            sends: AtomicUsize::new(0),
        });
        let drops = Arc::new(Drops::default());
        let (sender, _bulk) = Sender::new(transport, drops.clone());

        sender.send(header(), b"payload");

        assert_eq!(drops.snapshot().queue_full, 0);
    }
}
