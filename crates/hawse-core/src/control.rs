use std::io;

use futures_util::{SinkExt, StreamExt};
use hawse_proto::frame::{FrameError, codec, decode, encode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec, LengthDelimitedCodecError};

use crate::frame::StreamFrameError;
use crate::transport::{RecvHalf, SendHalf};

type Tx = FramedWrite<SendHalf, LengthDelimitedCodec>;
type Rx = FramedRead<RecvHalf, LengthDelimitedCodec>;

/// Whether a read failed on a length prefix over the frame limit. The codec says so the same way
/// over both transports, and it is told by its own error and not by `InvalidData`: under the TCP
/// transport TLS reports a record it cannot read with that kind too, and that is a broken stream.
fn over_limit(err: &io::Error) -> bool {
    err.get_ref()
        .and_then(|inner| inner.downcast_ref::<LengthDelimitedCodecError>())
        .is_some()
}

/// Owns framing only: the ping tick, nonce and liveness deadline stay in each session's
/// `select!` loop, where they interleave with events `Control` knows nothing about.
pub struct Control {
    tx: Tx,
    rx: Rx,
}

impl Control {
    pub fn new(send: SendHalf, recv: RecvHalf) -> Self {
        Self {
            tx: FramedWrite::new(send, codec()),
            rx: FramedRead::new(recv, codec()),
        }
    }

    /// `Io` means the stream itself is gone, `Frame` means `msg` doesn't fit one — callers that
    /// retry only on the former rely on the split staying that way.
    pub async fn send<T: Serialize>(&mut self, msg: &T) -> Result<(), StreamFrameError> {
        let bytes = encode(msg)?;
        self.tx.send(bytes).await?;
        Ok(())
    }

    pub async fn next<T: DeserializeOwned>(&mut self) -> Option<T> {
        self.try_next().await.ok().flatten()
    }

    /// `next` for a caller that tells a frame it cannot take from the stream ending. An error is
    /// something the peer wrote: a frame that does not decode, or a length prefix over the limit.
    /// `Ok(None)` is the end, whether the peer finished the stream or it broke, mid-frame
    /// included: a connection cut with a frame in flight is a peer that left, and neither
    /// transport can tell that from a peer that stopped short on purpose.
    pub async fn try_next<T: DeserializeOwned>(&mut self) -> Result<Option<T>, FrameError> {
        match self.rx.next().await {
            Some(Ok(frame)) => decode(&frame).map(Some),
            Some(Err(err)) if over_limit(&err) => {
                // The codec refuses a prefix without consuming it, so it still heads the buffer.
                let buffered = self.rx.read_buffer().first_chunk().copied();
                let prefix = u32::from_le_bytes(buffered.unwrap_or([u8::MAX; 4]));
                Err(FrameError::TooLarge(prefix as usize))
            }
            _ => Ok(None),
        }
    }

    /// Takes `&mut self`, not `self`, so the receive half survives the call. Consuming it here
    /// would drop an unfinished `RecvStream`, and quinn's `Drop` sends `STOP_SENDING` for an
    /// unfinished one — telling the peer to stop writing on a stream it may still have a
    /// legitimate reply in flight for (a `Pong` to a `Ping` this side already sent).
    pub async fn finish(&mut self) {
        self.tx.get_mut().finish().await;
    }
}
