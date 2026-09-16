use std::future::poll_fn;
use std::net::SocketAddr;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use bytes::Bytes;
use futures_util::future::BoxFuture;
use hawse_proto::key::PublicKey;
use rustls::pki_types::ServerName;
use socket2::{SockRef, TcpKeepalive};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::timeout;
use tokio_rustls::{TlsAcceptor, TlsConnector, TlsStream};
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use tokio_util::sync::CancellationToken;

use crate::tls;
use crate::transport::quic::Tuning;
use crate::transport::{CloseReason, RecvHalf, SendHalf, Transport, TransportError};

#[derive(Debug, thiserror::Error)]
#[error("the multiplexer is closed")]
struct Closed;

fn failed(err: impl std::error::Error + Send + Sync + 'static) -> TransportError {
    TransportError::Connection(Box::new(err))
}

fn config(tuning: Tuning) -> yamux::Config {
    let streams = tuning.max_streams as usize;
    // Every stream is guaranteed `DEFAULT_CREDIT`, and yamux grows a stream's window only into
    // whatever the connection limit has left over that guarantee. Set the limit to the guarantee
    // exactly and the slack is zero, pinning every stream at 256 KiB per round trip for the life
    // of the connection; the leftover here is QUIC's connection window, shared across streams and
    // handed out only to streams that earn it.
    let window = streams
        .saturating_mul(yamux::DEFAULT_CREDIT as usize)
        .saturating_add(usize::try_from(tuning.connection_window).unwrap_or(usize::MAX));
    let mut cfg = yamux::Config::default();
    // Each setter asserts against the other's current value, and the default limit of 1 GiB covers
    // only 4096 streams: lifting it first is what keeps a larger `max_streams` from panicking here.
    cfg.set_max_connection_receive_window(None);
    cfg.set_max_num_streams(streams);
    cfg.set_max_connection_receive_window(Some(window));
    cfg
}

fn prepare(socket: &TcpStream, tuning: Tuning) -> Result<(), TransportError> {
    socket.set_nodelay(true).map_err(failed)?;
    // The silent peer this transport exists for — a NAT that forgot the flow — never sends a FIN,
    // so without keepalive the driver parks on a socket the kernel would never fail.
    let keepalive = TcpKeepalive::new().with_time(tuning.idle_timeout);
    SockRef::from(socket)
        .set_tcp_keepalive(&keepalive)
        .map_err(failed)
}

/// Dial and handshake share one `Tuning::idle_timeout` deadline, so a black-holed address fails in
/// that long rather than in the kernel's own connect timeout.
pub async fn connect(
    remote: SocketAddr,
    tls: rustls::ClientConfig,
    tuning: Tuning,
) -> Result<TcpTransport, TransportError> {
    let handshake = async {
        let socket = TcpStream::connect(remote).await.map_err(failed)?;
        prepare(&socket, tuning)?;
        // The pinned-key verifier ignores server names.
        let name = ServerName::try_from("hawse").expect("literal server name");
        TlsConnector::from(Arc::new(tls))
            .connect(name, socket)
            .await
            .map_err(failed)
    };
    let stream = timeout(tuning.idle_timeout, handshake)
        .await
        .map_err(failed)??;
    TcpTransport::spawn(stream.into(), yamux::Mode::Client, tuning)
}

pub async fn accept(
    socket: TcpStream,
    tls: Arc<rustls::ServerConfig>,
    tuning: Tuning,
) -> Result<TcpTransport, TransportError> {
    prepare(&socket, tuning)?;
    let stream = timeout(tuning.idle_timeout, TlsAcceptor::from(tls).accept(socket))
        .await
        .map_err(failed)?
        .map_err(failed)?;
    TcpTransport::spawn(stream.into(), yamux::Mode::Server, tuning)
}

type OpenRequest = oneshot::Sender<Result<yamux::Stream, TransportError>>;

/// One TLS connection multiplexed with yamux, for networks that drop QUIC's UDP.
///
/// Dropping it closes the connection. Unlike quinn's streams, the halves handed out by `open_bi`
/// and `accept_bi` do not keep it alive, so a caller still pumping them must hold it too.
///
/// The caller is the only thing bounding concurrent streams to `Tuning::max_streams`. yamux answers
/// the one past the limit by tearing the whole connection down, inbound and outbound alike, so the
/// error `open_bi` returns there is not a per-stream failure — the transport is already dead.
///
/// Liveness is the kernel's: a peer that vanishes silently is noticed after `Tuning::idle_timeout`
/// of quiet *plus* the OS keepalive probe schedule, not within `idle_timeout` the way QUIC's idle
/// timer bounds it. There is no hawse-level heartbeat on this transport.
#[derive(Debug)]
pub struct TcpTransport {
    open: mpsc::Sender<OpenRequest>,
    inbound: Mutex<mpsc::UnboundedReceiver<yamux::Stream>>,
    closed: CancellationToken,
    remote: SocketAddr,
    peer_key: Option<PublicKey>,
}

impl TcpTransport {
    fn spawn(
        stream: TlsStream<TcpStream>,
        mode: yamux::Mode,
        tuning: Tuning,
    ) -> Result<Self, TransportError> {
        let (remote, peer_key) = {
            let (socket, state) = stream.get_ref();
            let key = state
                .peer_certificates()
                .and_then(<[_]>::first)
                .and_then(|cert| tls::peer_key(cert).ok());
            (socket.peer_addr().map_err(failed)?, key)
        };
        // Bounded by the streams that could ever be outstanding, floored at 1 because tokio rejects
        // a zero-capacity channel.
        let (open, requests) = mpsc::channel((tuning.max_streams as usize).max(1));
        let (deliver, inbound) = mpsc::unbounded_channel();
        let closed = CancellationToken::new();
        tokio::spawn(
            Driver {
                conn: yamux::Connection::new(stream.compat(), config(tuning), mode),
                requests,
                pending: None,
                deliver,
                closed: closed.clone(),
                grace: tuning.idle_timeout,
            }
            .run(),
        );
        Ok(Self {
            open,
            inbound: Mutex::new(inbound),
            closed,
            remote,
            peer_key,
        })
    }
}

impl Transport for TcpTransport {
    fn open_bi(&self) -> BoxFuture<'_, Result<(SendHalf, RecvHalf), TransportError>> {
        Box::pin(async move {
            let (tx, rx) = oneshot::channel();
            self.open.send(tx).await.map_err(|_| failed(Closed))?;
            Ok(halves(rx.await.map_err(|_| failed(Closed))??))
        })
    }

    fn accept_bi(&self) -> BoxFuture<'_, Result<(SendHalf, RecvHalf), TransportError>> {
        Box::pin(async move {
            let stream = self.inbound.lock().await.recv().await;
            stream.map(halves).ok_or_else(|| failed(Closed))
        })
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

    /// yamux's go-away carries no room for `reason`, so the peer learns only that we are done.
    fn close(&self, reason: CloseReason) {
        tracing::debug!(?reason, "closing the tcp transport");
        self.closed.cancel();
    }

    fn closed(&self) -> BoxFuture<'_, ()> {
        Box::pin(self.closed.cancelled())
    }

    fn remote_address(&self) -> SocketAddr {
        self.remote
    }

    fn peer_key(&self) -> Option<PublicKey> {
        self.peer_key
    }
}

fn halves(stream: yamux::Stream) -> (SendHalf, RecvHalf) {
    let (recv, send) = tokio::io::split(stream.compat());
    (SendHalf::Tcp(send), RecvHalf::Tcp(recv))
}

enum Step {
    Inbound(yamux::Stream),
    Done,
}

/// The `yamux::Connection` moves data on every stream only while it is being polled, so this task
/// must hold it alone and must never stop.
struct Driver {
    conn: yamux::Connection<Compat<TlsStream<TcpStream>>>,
    requests: mpsc::Receiver<OpenRequest>,
    pending: Option<OpenRequest>,
    deliver: mpsc::UnboundedSender<yamux::Stream>,
    closed: CancellationToken,
    grace: Duration,
}

impl Driver {
    async fn run(mut self) {
        let closed = self.closed.clone();
        tokio::select! {
            () = closed.cancelled() => {}
            () = self.serve() => {}
        }
        // yamux flushes every queued frame before its go-away, which on a wedged socket is
        // unbounded. Both are dropped first so a caller queued for a stream fails now, not then.
        drop(self.requests);
        drop(self.pending);
        let goodbye = poll_fn(|cx| self.conn.poll_close(cx));
        let _: Result<yamux::Result<()>, _> = timeout(self.grace, goodbye).await;
        closed.cancel();
    }

    async fn serve(&mut self) {
        loop {
            let step = poll_fn(|cx| self.poll_step(cx)).await;
            match step {
                Step::Inbound(stream) => {
                    if self.deliver.send(stream).is_err() {
                        return;
                    }
                }
                Step::Done => return,
            }
        }
    }

    /// `poll_new_outbound` parks once 256 streams are unacknowledged, and the acknowledgement that
    /// releases it arrives only through `poll_next_inbound` — so awaiting the former in a `select!`
    /// arm of its own deadlocks against the peer.
    fn poll_step(&mut self, cx: &mut Context<'_>) -> Poll<Step> {
        loop {
            if self.pending.is_none() {
                match self.requests.poll_recv(cx) {
                    Poll::Ready(Some(request)) => self.pending = Some(request),
                    Poll::Ready(None) => return Poll::Ready(Step::Done),
                    Poll::Pending => {}
                }
            }
            if self.pending.is_some()
                && let Poll::Ready(opened) = self.conn.poll_new_outbound(cx)
            {
                let request = self.pending.take().expect("checked above");
                let _: Result<(), _> = request.send(opened.map_err(failed));
                continue;
            }
            return Poll::Ready(match ready!(self.conn.poll_next_inbound(cx)) {
                Some(Ok(stream)) => Step::Inbound(stream),
                Some(Err(err)) => {
                    tracing::debug!(%err, "multiplexer failed");
                    Step::Done
                }
                None => Step::Done,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `yamux::Config` has no getters, so the numbers are read back out of its `Debug`.
    fn field(printed: &str, key: &str) -> usize {
        printed
            .split(key)
            .nth(1)
            .expect("field present")
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .expect("decimal field")
    }

    #[test]
    fn the_connection_window_leaves_a_stream_room_to_grow() {
        for tuning in [Tuning::SERVER, Tuning::CLIENT] {
            let printed = format!("{:?}", config(tuning));
            let window = field(&printed, "max_connection_receive_window: Some(");
            let streams = field(&printed, "max_num_streams: ");
            assert!(
                window > streams * yamux::DEFAULT_CREDIT as usize,
                "equality pins every stream at DEFAULT_CREDIT for the life of the connection: \
                 {printed}"
            );
        }
    }

    #[test]
    fn a_stream_limit_beyond_yamuxs_default_window_does_not_panic() {
        let _ = config(Tuning {
            max_streams: 8192,
            ..Tuning::CLIENT
        });
    }
}
