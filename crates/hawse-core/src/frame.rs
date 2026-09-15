use hawse_proto::frame::{self, FrameError, MAX_FRAME};
use quinn::{RecvStream, SendStream};
use serde::Serialize;
use serde::de::DeserializeOwned;

#[derive(Debug, thiserror::Error)]
pub enum StreamFrameError {
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error("stream write failed")]
    Write(#[from] quinn::WriteError),
    #[error("stream read failed")]
    Read(#[from] quinn::ReadExactError),
}

/// Reads exactly one frame and leaves the stream positioned at the first byte after it, unlike a codec, which reads ahead.
pub async fn read_frame<T: DeserializeOwned>(recv: &mut RecvStream) -> Result<T, StreamFrameError> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len).into());
    }
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body).await?;
    Ok(frame::decode(&body)?)
}

pub async fn write_frame<T: Serialize>(
    send: &mut SendStream,
    value: &T,
) -> Result<(), StreamFrameError> {
    let body = frame::encode(value)?;
    let len = u32::try_from(body.len()).expect("encode caps frames below u32::MAX");
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&body);
    send.write_all(&buf).await?;
    Ok(())
}
