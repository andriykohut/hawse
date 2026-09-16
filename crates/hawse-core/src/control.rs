use futures_util::{SinkExt, StreamExt};
use hawse_proto::frame::{codec, decode, encode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

use crate::frame::StreamFrameError;
use crate::transport::{RecvHalf, SendHalf};

type Tx = FramedWrite<SendHalf, LengthDelimitedCodec>;
type Rx = FramedRead<RecvHalf, LengthDelimitedCodec>;

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
        let frame = self.rx.next().await?.ok()?;
        decode(&frame).ok()
    }

    /// Takes `&mut self`, not `self`, so the receive half survives the call. Consuming it here
    /// would drop an unfinished `RecvStream`, and quinn's `Drop` sends `STOP_SENDING` for an
    /// unfinished one — telling the peer to stop writing on a stream it may still have a
    /// legitimate reply in flight for (a `Pong` to a `Ping` this side already sent).
    pub async fn finish(&mut self) {
        self.tx.get_mut().finish().await;
    }
}
