use std::future::poll_fn;
use std::net::SocketAddr;
use std::sync::Arc;
use std::task::{Context, Poll, ready};

use bytes::Bytes;
use futures_util::future::BoxFuture;
use hawse_proto::key::PublicKey;
use rustls::pki_types::ServerName;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_rustls::{TlsAcceptor, TlsConnector, TlsStream};
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use tokio_util::sync::CancellationToken;

use crate::tls;
use crate::transport::quic::Tuning;
use crate::transport::{CloseReason, RecvHalf, SendHalf, Transport, TransportError};

/// The same budget QUIC grants, so falling back does not change how many visitors a client carries.
const MAX_STREAMS: usize = Tuning::SERVER.max_streams as usize;

#[derive(Debug, thiserror::Error)]
#[error("the multiplexer is closed")]
struct Closed;

fn failed(err: impl std::error::Error + Send + Sync + 'static) -> TransportError {
    TransportError::Connection(Box::new(err))
}

fn config() -> yamux::Config {
    let mut cfg = yamux::Config::default();
    // yamux panics unless the connection window covers one default credit per stream.
    cfg.set_max_connection_receive_window(Some(MAX_STREAMS * yamux::DEFAULT_CREDIT as usize));
    cfg.set_max_num_streams(MAX_STREAMS);
    cfg
}

pub async fn connect(
    remote: SocketAddr,
    tls: rustls::ClientConfig,
) -> Result<TcpTransport, TransportError> {
    let socket = TcpStream::connect(remote).await.map_err(failed)?;
    socket.set_nodelay(true).map_err(failed)?;
    // The pinned-key verifier ignores server names.
    let name = ServerName::try_from("hawse").expect("literal server name");
    let stream = TlsConnector::from(Arc::new(tls))
        .connect(name, socket)
        .await
        .map_err(failed)?;
    TcpTransport::spawn(stream.into(), yamux::Mode::Client)
}

pub async fn accept(
    socket: TcpStream,
    tls: Arc<rustls::ServerConfig>,
) -> Result<TcpTransport, TransportError> {
    socket.set_nodelay(true).map_err(failed)?;
    let stream = TlsAcceptor::from(tls)
        .accept(socket)
        .await
        .map_err(failed)?;
    TcpTransport::spawn(stream.into(), yamux::Mode::Server)
}

type OpenRequest = oneshot::Sender<Result<yamux::Stream, TransportError>>;

/// One TLS connection multiplexed with yamux, for networks that drop QUIC's UDP.
///
/// Dropping it closes the connection. Unlike quinn's streams, the halves handed out by `open_bi`
/// and `accept_bi` do not keep it alive, so a caller still pumping them must hold it too.
#[derive(Debug)]
pub struct TcpTransport {
    open: mpsc::Sender<OpenRequest>,
    inbound: Mutex<mpsc::UnboundedReceiver<yamux::Stream>>,
    closed: CancellationToken,
    remote: SocketAddr,
    peer_key: Option<PublicKey>,
}

impl TcpTransport {
    fn spawn(stream: TlsStream<TcpStream>, mode: yamux::Mode) -> Result<Self, TransportError> {
        let (remote, peer_key) = {
            let (socket, state) = stream.get_ref();
            let key = state
                .peer_certificates()
                .and_then(<[_]>::first)
                .and_then(|cert| tls::peer_key(cert).ok());
            (socket.peer_addr().map_err(failed)?, key)
        };
        let (open, requests) = mpsc::channel(MAX_STREAMS);
        let (deliver, inbound) = mpsc::unbounded_channel();
        let closed = CancellationToken::new();
        tokio::spawn(
            Driver {
                conn: yamux::Connection::new(stream.compat(), config(), mode),
                requests,
                pending: None,
                deliver,
                closed: closed.clone(),
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
}

impl Driver {
    async fn run(mut self) {
        let closed = self.closed.clone();
        tokio::select! {
            () = closed.cancelled() => {}
            () = self.serve() => {}
        }
        let _: yamux::Result<()> = poll_fn(|cx| self.conn.poll_close(cx)).await;
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

    /// `poll_new_outbound` parks once 256 streams are unacknowledged, and only `poll_next_inbound`
    /// collects the acknowledgements, so awaiting either one alone deadlocks.
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
