pub mod quic;

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_util::future::BoxFuture;
use hawse_proto::key::PublicKey;
use quinn::VarInt;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// The code travels to the peer, so these numbers are part of the protocol, not an internal enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    Shutdown,
    Superseded,
    Fatal,
}

impl CloseReason {
    pub fn code(self) -> u32 {
        match self {
            Self::Shutdown => 0,
            Self::Superseded => 1,
            Self::Fatal => 2,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("connection failed")]
    Connection(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("this transport has no datagrams")]
    NoDatagrams,
    #[error("stream i/o failed")]
    Io(#[source] io::Error),
}

/// `finish` ends the stream cleanly and the peer sees EOF; `reset` aborts it, so the peer sees a
/// reset rather than a truncated payload delivered as complete.
#[derive(Debug)]
pub enum SendHalf {
    Quic(quinn::SendStream),
}

impl SendHalf {
    /// Writes all of `data`. Owned rather than borrowed so quinn can take the buffer without copying it.
    pub async fn write_bytes(&mut self, data: Bytes) -> io::Result<()> {
        match self {
            Self::Quic(send) => send.write_chunk(data).await.map_err(io::Error::from),
        }
    }

    pub fn reset(&mut self, code: u32) {
        match self {
            Self::Quic(send) => {
                let _: Result<(), quinn::ClosedStream> = send.reset(VarInt::from_u32(code));
            }
        }
    }

    pub async fn finish(&mut self) {
        let _: io::Result<()> = tokio::io::AsyncWriteExt::shutdown(self).await;
    }
}

#[derive(Debug)]
pub enum RecvHalf {
    Quic(quinn::RecvStream),
}

impl RecvHalf {
    /// Hands back quinn's own buffer, so the stream-to-socket path copies once, not twice. `None`
    /// once the peer has finished the stream.
    pub async fn read_bytes(&mut self, max: usize) -> io::Result<Option<Bytes>> {
        match self {
            Self::Quic(recv) => recv
                .read_chunk(max, true)
                .await
                .map(|chunk| chunk.map(|chunk| chunk.bytes))
                .map_err(io::Error::from),
        }
    }

    pub fn stop(&mut self, code: u32) {
        match self {
            Self::Quic(recv) => {
                let _: Result<(), quinn::ClosedStream> = recv.stop(VarInt::from_u32(code));
            }
        }
    }
}

// quinn's `SendStream` carries an inherent `poll_write` that shadows this one and returns quinn's
// own error type.
impl AsyncWrite for SendHalf {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Quic(send) => AsyncWrite::poll_write(Pin::new(send), cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Quic(send) => AsyncWrite::poll_flush(Pin::new(send), cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Quic(send) => AsyncWrite::poll_shutdown(Pin::new(send), cx),
        }
    }
}

// quinn's `RecvStream` carries an inherent `poll_read` that shadows this one and takes a plain
// `&mut [u8]`.
impl AsyncRead for RecvHalf {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Quic(recv) => AsyncRead::poll_read(Pin::new(recv), cx, buf),
        }
    }
}

/// Returns boxed futures rather than `async fn` so `dyn Transport` is object-safe; that costs one
/// allocation per stream, never one per byte.
pub trait Transport: Send + Sync + 'static {
    fn open_bi(&self) -> BoxFuture<'_, Result<(SendHalf, RecvHalf), TransportError>>;
    fn accept_bi(&self) -> BoxFuture<'_, Result<(SendHalf, RecvHalf), TransportError>>;
    fn send_datagram(&self, data: Bytes) -> Result<(), TransportError>;
    fn recv_datagram(&self) -> BoxFuture<'_, Result<Bytes, TransportError>>;
    /// `None` when this connection cannot carry datagrams, and the limit can move with the path MTU,
    /// so it is not safe to cache.
    fn max_datagram_size(&self) -> Option<usize>;
    /// Discards stream data still in flight, so anything the peer must read has to land first.
    fn close(&self, reason: CloseReason);
    /// Ends when either side closes, so waiting here before `close` gives the peer a chance to read
    /// what is still in flight.
    fn closed(&self) -> BoxFuture<'_, ()>;
    fn remote_address(&self) -> SocketAddr;
    fn peer_key(&self) -> Option<PublicKey>;
}
