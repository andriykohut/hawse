use bytes::BytesMut;
use hawse_proto::msg::reset::ABORTED;
use tokio::io::{
    AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf,
};
use tokio::net::TcpStream;

use crate::error::chain;
use crate::transport::{RecvHalf, SendHalf, reset_code};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub to_stream: u64,
    pub to_socket: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum PumpError {
    #[error("socket failed")]
    Socket(#[source] std::io::Error),
    #[error("stream failed")]
    Stream(#[source] std::io::Error),
}

impl PumpError {
    /// The code the far end reset the stream with, when that is what ended the pump.
    pub fn reset_code(&self) -> Option<u32> {
        match self {
            Self::Stream(err) => reset_code(err),
            Self::Socket(_) => None,
        }
    }
}

/// What `pump` needs from the socket at the tunnel's edge, beyond reading and writing it.
pub trait Edge: AsyncRead + AsyncWrite + Unpin + Send + 'static {
    /// Makes the close that follows a reset, not a clean end.
    fn abort(&self);
}

impl Edge for TcpStream {
    /// `SO_LINGER` of zero: the kernel answers the close with an RST and drops what it had queued.
    fn abort(&self) {
        if let Err(err) = self.set_zero_linger() {
            // Without it the close is a FIN, and the visitor reads a clean end.
            tracing::debug!(err = %chain(&err), "cannot make the close an RST");
        }
    }
}

/// An in-memory pipe has no reset to send; closing is all its peer can see.
impl Edge for DuplexStream {
    fn abort(&self) {}
}

/// The socket's half of what `ResetOnDrop` does for the stream. A dropped `TcpStream` closes with
/// a FIN, which hands the application a clean end on a transfer that was cut short, so unless
/// both directions finished the socket is aborted first.
struct AbortOnDrop<S: Edge> {
    halves: Option<(ReadHalf<S>, WriteHalf<S>)>,
    finished: bool,
}

impl<S: Edge> Drop for AbortOnDrop<S> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Some((reader, writer)) = self.halves.take() {
            reader.unsplit(writer).abort();
        }
    }
}

// quinn's own `Drop for SendStream` finishes (not resets) a stream that's dropped mid-transfer,
// so try_join! cancelling this half on the other side's error would otherwise hand the peer a
// clean end-of-stream on a truncated payload.
struct ResetOnDrop(Option<SendHalf>);

impl ResetOnDrop {
    fn stream(&mut self) -> &mut SendHalf {
        self.0.as_mut().expect("not yet taken")
    }

    fn take(mut self) -> SendHalf {
        self.0.take().expect("not yet taken")
    }
}

impl Drop for ResetOnDrop {
    fn drop(&mut self) {
        if let Some(mut send) = self.0.take() {
            send.reset(ABORTED);
        }
    }
}

/// Ends when both directions have delivered EOF. Any other ending aborts both sides: the stream is
/// reset, so the peer's read fails on either transport, and the socket is closed with a reset, so
/// the application behind it does not read a truncated transfer as a whole one. A pump dropped
/// before it ends counts as any other ending.
pub async fn pump<S: Edge>(
    socket: S,
    send: SendHalf,
    mut recv: RecvHalf,
    buffer: usize,
) -> Result<Stats, PumpError> {
    let mut edge = AbortOnDrop {
        halves: Some(tokio::io::split(socket)),
        finished: false,
    };
    let (reader, writer) = edge.halves.as_mut().expect("set just above");

    let to_stream = async move {
        let mut total = 0u64;
        let mut buf = BytesMut::with_capacity(buffer);
        let mut send = ResetOnDrop(Some(send));
        loop {
            buf.reserve(buffer);
            let n = reader.read_buf(&mut buf).await.map_err(PumpError::Socket)?;
            if n == 0 {
                break;
            }
            total += u64::try_from(n).expect("usize fits u64");
            send.stream()
                .write_bytes(buf.split().freeze())
                .await
                .map_err(PumpError::Stream)?;
        }
        send.take().finish().await;
        Ok::<u64, PumpError>(total)
    };

    let to_socket = async move {
        let mut total = 0u64;
        // Reading into this reused buffer measured the same as quinn's zero-copy read_chunk.
        let mut buf = vec![0u8; buffer];
        loop {
            let n = recv.read(&mut buf).await.map_err(PumpError::Stream)?;
            if n == 0 {
                break;
            }
            total += u64::try_from(n).expect("usize fits u64");
            writer
                .write_all(&buf[..n])
                .await
                .map_err(PumpError::Socket)?;
        }
        let _ = writer.shutdown().await;
        Ok::<u64, PumpError>(total)
    };

    let (to_stream, to_socket) = tokio::try_join!(to_stream, to_socket)?;
    edge.finished = true;
    Ok(Stats {
        to_stream,
        to_socket,
    })
}
