pub mod quic;
pub mod records;
pub mod tcp;

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_util::future::BoxFuture;
use hawse_proto::key::PublicKey;
use hawse_proto::msg::close;
use quinn::VarInt;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, ReadHalf, WriteHalf};
use tokio_util::compat::Compat;

use records::{RecordReader, RecordWriter};

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
    #[error("payload does not fit a datagram on this path")]
    DatagramTooLarge,
    #[error("stream i/o failed")]
    Io(#[source] io::Error),
}

/// `finish` ends the stream cleanly and the peer sees EOF; `reset` aborts it, so the peer sees a
/// reset rather than a truncated payload delivered as complete.
///
/// yamux cannot say which happened, so `Tcp` writes it into the stream: see `records`.
#[derive(Debug)]
pub enum SendHalf {
    Quic(quinn::SendStream),
    Tcp(RecordWriter<WriteHalf<Compat<yamux::Stream>>>),
}

impl SendHalf {
    /// Writes all of `data`. Owned rather than borrowed so quinn can take the buffer without copying it.
    pub async fn write_bytes(&mut self, data: Bytes) -> io::Result<()> {
        match self {
            Self::Quic(send) => send.write_chunk(data).await.map_err(io::Error::from),
            // Flushed: the writer accepts a record before yamux has taken it, and nothing says
            // when this stream's next write will come.
            Self::Tcp(send) => {
                tokio::io::AsyncWriteExt::write_all(send, &data).await?;
                tokio::io::AsyncWriteExt::flush(send).await
            }
        }
    }

    /// On `Tcp` the code is best effort, since this runs in `Drop`s and gets one poll. The abort
    /// is not: a stream that ends without a finish record reads as reset. The peer reads it once
    /// the stream is dropped, which needs the read half gone too.
    pub fn reset(&mut self, code: u32) {
        match self {
            Self::Quic(send) => {
                let _: Result<(), quinn::ClosedStream> = send.reset(VarInt::from_u32(code));
            }
            Self::Tcp(send) => send.reset(code),
        }
    }

    /// `reset`, waiting on `Tcp` until the code has left. A stalled link holds it, so bound it.
    pub async fn reset_flushed(&mut self, code: u32) {
        match self {
            Self::Quic(_) => self.reset(code),
            Self::Tcp(send) => {
                let _: io::Result<()> = send.reset_flushed(code).await;
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
    Tcp(RecordReader<ReadHalf<Compat<yamux::Stream>>>),
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

/// The code a peer reset or stopped a stream with, when the error carries one. On QUIC a failed
/// read and a failed write both can; on TCP only a read that met a reset record does.
pub fn reset_code(err: &io::Error) -> Option<u32> {
    let source = err.get_ref()?;
    if let Some(reset) = source.downcast_ref::<records::StreamReset>() {
        return reset.code;
    }
    let code = match source.downcast_ref::<quinn::ReadError>() {
        Some(quinn::ReadError::Reset(code)) => *code,
        _ => match source.downcast_ref::<quinn::WriteError>() {
            Some(quinn::WriteError::Stopped(code)) => *code,
            _ => return None,
        },
    };
    u32::try_from(code.into_inner()).ok()
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
    /// Bytes free in the outgoing datagram queue. A transport without datagrams returns `0`.
    fn datagram_send_buffer_space(&self) -> usize;
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

    /// Against literals rather than the constants: these numbers go out on a QUIC close frame, so a
    /// reason pointed at a different constant has to fail here and not just stay distinct.
    #[test]
    fn every_close_reason_keeps_its_wire_code() {
        let expected = [
            (CloseReason::Shutdown, 0x00),
            (CloseReason::Superseded, 0x01),
            (CloseReason::Denied, 0x02),
            (CloseReason::NoKey, 0x03),
            (CloseReason::NoHello, 0x04),
            (CloseReason::BadHello, 0x05),
            (CloseReason::DuplicateHello, 0x06),
            (CloseReason::Unresponsive, 0x07),
            (CloseReason::PeerLeft, 0x08),
            (CloseReason::ControlClosed, 0x09),
        ];
        assert_eq!(expected.len(), EVERY_REASON.len());
        for (reason, code) in expected {
            assert_eq!(reason.code(), code, "{reason:?}");
        }
    }

    #[test]
    fn transport_kinds_print_in_lowercase() {
        assert_eq!(TransportKind::Quic.to_string(), "quic");
        assert_eq!(TransportKind::Tcp.to_string(), "tcp");
    }

    #[test]
    fn a_reset_code_is_read_out_of_either_transports_error() {
        let code = VarInt::from_u32(0x11);
        let quic_read = io::Error::from(quinn::ReadError::Reset(code));
        let quic_write = io::Error::from(quinn::WriteError::Stopped(code));
        let tcp = io::Error::new(
            io::ErrorKind::ConnectionReset,
            records::StreamReset { code: Some(0x11) },
        );
        assert_eq!(reset_code(&quic_read), Some(0x11));
        assert_eq!(reset_code(&quic_write), Some(0x11));
        assert_eq!(reset_code(&tcp), Some(0x11));
    }

    #[test]
    fn an_error_without_a_code_has_none() {
        let bare = io::Error::new(
            io::ErrorKind::ConnectionReset,
            records::StreamReset { code: None },
        );
        assert_eq!(reset_code(&bare), None);
        assert_eq!(reset_code(&io::Error::other("boom")), None);
        assert_eq!(
            reset_code(&io::Error::from(quinn::ReadError::ClosedStream)),
            None
        );
    }
}
