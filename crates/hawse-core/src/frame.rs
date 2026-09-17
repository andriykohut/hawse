use hawse_proto::frame::{self, FrameError, MAX_FRAME};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::transport::{RecvHalf, SendHalf};

#[derive(Debug, thiserror::Error)]
pub enum StreamFrameError {
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error("stream i/o failed")]
    Io(#[from] std::io::Error),
}

/// Reads exactly one frame and leaves the stream positioned at the first byte after it, unlike a codec, which reads ahead.
pub async fn read_frame<T: DeserializeOwned>(recv: &mut RecvHalf) -> Result<T, StreamFrameError> {
    Ok(frame::decode(&read_body(recv).await?)?)
}

pub async fn read_body(recv: &mut RecvHalf) -> Result<Vec<u8>, StreamFrameError> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len).into());
    }
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body).await?;
    Ok(body)
}

/// Matches `hawse_proto::frame::codec()`'s wire format bit-for-bit, so the two are interchangeable across a stream.
pub async fn write_frame<T: Serialize>(
    send: &mut SendHalf,
    value: &T,
) -> Result<(), StreamFrameError> {
    write_body(send, &frame::encode(value)?).await
}

/// `body` must already be capped at `MAX_FRAME`; both encoders do that.
pub async fn write_body(send: &mut SendHalf, body: &[u8]) -> Result<(), StreamFrameError> {
    let len = u32::try_from(body.len()).expect("encoders cap bodies below u32::MAX");
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(body);
    send.write_all(&buf).await?;
    Ok(())
}
