pub mod quic;
pub mod tcp;

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use bytes::Bytes;
use futures_util::future::BoxFuture;
use hawse_proto::key::PublicKey;
use hawse_proto::msg::close;
use quinn::VarInt;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, ReadHalf, WriteHalf};
use tokio_util::compat::Compat;

/// Why a connection is being closed. The code and the text both travel to a QUIC peer, so they are
/// part of the protocol; on the TCP transport neither does, and only the local log keeps them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    Shutdown,
    Superseded,
    Denied,
    NoKey,
    NoHello,
    BadHello,
    DuplicateHello,
    Unresponsive,
    PeerLeft,
    ControlClosed,
}

impl CloseReason {
    pub fn code(self) -> u32 {
        match self {
            Self::Shutdown => close::SHUTDOWN,
            Self::Superseded => close::SUPERSEDED,
            Self::Denied => close::DENIED,
            Self::NoKey => close::NO_KEY,
            Self::NoHello => close::NO_HELLO,
            Self::BadHello => close::BAD_HELLO,
            Self::DuplicateHello => close::DUPLICATE_HELLO,
            Self::Unresponsive => close::UNRESPONSIVE,
            Self::PeerLeft => close::PEER_LEFT,
            Self::ControlClosed => close::CONTROL_CLOSED,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shutdown => "shutdown",
            Self::Superseded => "superseded",
            Self::Denied => "denied",
            Self::NoKey => "no key",
            Self::NoHello => "no hello",
            Self::BadHello => "expected hello",
            Self::DuplicateHello => "duplicate hello",
            Self::Unresponsive => "unresponsive",
            Self::PeerLeft => "peer left",
            Self::ControlClosed => "control stream closed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportKind {
    Quic,
    Tcp,
}

impl fmt::Display for TransportKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Quic => "quic",
            Self::Tcp => "tcp",
        })
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
///
/// `Tcp` cannot keep that second promise: yamux has no per-stream error code and hands a reset
/// stream to its reader as end-of-stream, so a peer cannot tell an abort from a clean finish.
#[derive(Debug)]
pub enum SendHalf {
    Quic(quinn::SendStream),
    Tcp(WriteHalf<Compat<yamux::Stream>>),
}

impl SendHalf {
    /// Writes all of `data`. Owned rather than borrowed so quinn can take the buffer without copying it.
    pub async fn write_bytes(&mut self, data: Bytes) -> io::Result<()> {
        match self {
            Self::Quic(send) => send.write_chunk(data).await.map_err(io::Error::from),
            Self::Tcp(send) => tokio::io::AsyncWriteExt::write_all(send, &data).await,
        }
    }

    pub fn reset(&mut self, code: u32) {
        match self {
            Self::Quic(send) => {
                let _: Result<(), quinn::ClosedStream> = send.reset(VarInt::from_u32(code));
            }
            // Best effort: the one poll is Pending only if yamux's command channel is full, and
            // the close then still reaches the peer when the stream drops, as a RST not this FIN.
            Self::Tcp(send) => {
                let mut cx = Context::from_waker(Waker::noop());
                let _: Poll<io::Result<()>> = AsyncWrite::poll_shutdown(Pin::new(send), &mut cx);
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
    Tcp(ReadHalf<Compat<yamux::Stream>>),
}

impl RecvHalf {
    /// yamux has no equivalent of QUIC's `STOP_SENDING`, so on `Tcp` this does nothing and the peer
    /// keeps writing until the stream is dropped.
    pub fn stop(&mut self, code: u32) {
        match self {
            Self::Quic(recv) => {
                let _: Result<(), quinn::ClosedStream> = recv.stop(VarInt::from_u32(code));
            }
            Self::Tcp(_) => {}
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
            Self::Tcp(send) => AsyncWrite::poll_write(Pin::new(send), cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Quic(send) => AsyncWrite::poll_flush(Pin::new(send), cx),
            Self::Tcp(send) => AsyncWrite::poll_flush(Pin::new(send), cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Quic(send) => AsyncWrite::poll_shutdown(Pin::new(send), cx),
            Self::Tcp(send) => AsyncWrite::poll_shutdown(Pin::new(send), cx),
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
            Self::Tcp(recv) => AsyncRead::poll_read(Pin::new(recv), cx, buf),
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
    /// A QUIC peer loses stream data still in flight; yamux flushes what is queued before its
    /// go-away. Anything the peer must read has to land first either way.
    fn close(&self, reason: CloseReason);
    /// Ends when either side closes, so waiting here before `close` gives the peer a chance to read
    /// what is still in flight.
    fn closed(&self) -> BoxFuture<'_, ()>;
    fn remote_address(&self) -> SocketAddr;
    fn peer_key(&self) -> Option<PublicKey>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    const EVERY_REASON: [CloseReason; 10] = [
        CloseReason::Shutdown,
        CloseReason::Superseded,
        CloseReason::Denied,
        CloseReason::NoKey,
        CloseReason::NoHello,
        CloseReason::BadHello,
        CloseReason::DuplicateHello,
        CloseReason::Unresponsive,
        CloseReason::PeerLeft,
        CloseReason::ControlClosed,
    ];

    #[test]
    fn no_two_close_reasons_share_a_code_or_a_name() {
        let codes: BTreeSet<u32> = EVERY_REASON.iter().map(|r| r.code()).collect();
        let names: BTreeSet<&str> = EVERY_REASON.iter().map(|r| r.as_str()).collect();
        assert_eq!(codes.len(), EVERY_REASON.len(), "{codes:?}");
        assert_eq!(names.len(), EVERY_REASON.len(), "{names:?}");
    }

    #[test]
    fn transport_kinds_print_in_lowercase() {
        assert_eq!(TransportKind::Quic.to_string(), "quic");
        assert_eq!(TransportKind::Tcp.to_string(), "tcp");
    }
}
