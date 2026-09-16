pub mod quic;

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_util::future::BoxFuture;
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
    Io(#[source] std::io::Error),
}

/// `finish` ends the stream cleanly and the peer sees EOF; `reset` aborts it, so the peer sees a
/// reset rather than a truncated payload delivered as complete.
pub enum SendHalf {
    Quic(quinn::SendStream),
}

impl SendHalf {
    pub fn reset(&mut self, code: u32) {
        match self {
            Self::Quic(send) => {
                let _: Result<(), quinn::ClosedStream> = send.reset(VarInt::from_u32(code));
            }
        }
    }

    #[allow(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "async so a transport that cannot end a stream synchronously fits the signature"
    )]
    pub async fn finish(&mut self) {
        match self {
            Self::Quic(send) => {
                let _: Result<(), quinn::ClosedStream> = send.finish();
            }
        }
    }
}

pub enum RecvHalf {
    Quic(quinn::RecvStream),
}

impl RecvHalf {
    pub fn stop(&mut self, code: u32) {
        match self {
            Self::Quic(recv) => {
                let _: Result<(), quinn::ClosedStream> = recv.stop(VarInt::from_u32(code));
            }
        }
    }
}

// quinn's streams carry inherent `poll_write`/`poll_read` that shadow these trait methods and
// return quinn's own error type.
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
    fn max_datagram_size(&self) -> Option<usize>;
    fn close(&self, reason: CloseReason);
}
