use bytes::Bytes;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio_util::codec::LengthDelimitedCodec;

pub const MAX_FRAME: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("message could not be encoded")]
    Encode(#[source] postcard::Error),
    #[error("frame is malformed")]
    Decode(#[source] postcard::Error),
    #[error("frame of {0} bytes exceeds the {MAX_FRAME} byte limit")]
    TooLarge(usize),
}

pub fn codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .little_endian()
        .length_field_type::<u32>()
        .max_frame_length(MAX_FRAME)
        .new_codec()
}

pub fn encode<T: Serialize>(value: &T) -> Result<Bytes, FrameError> {
    let bytes = postcard::to_allocvec(value).map_err(FrameError::Encode)?;
    if bytes.len() > MAX_FRAME {
        return Err(FrameError::TooLarge(bytes.len()));
    }
    Ok(Bytes::from(bytes))
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, FrameError> {
    postcard::from_bytes(bytes).map_err(FrameError::Decode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msg::ClientMessage;
    use futures_util::{SinkExt, StreamExt};
    use tokio_util::codec::{FramedRead, FramedWrite};

    #[test]
    fn rejects_oversized_frames() {
        let big = ClientMessage::Hello {
            name: None,
            agent: "x".repeat(MAX_FRAME),
        };
        assert!(matches!(encode(&big), Err(FrameError::TooLarge(_))));
    }

    #[test]
    fn rejects_truncated_bytes() {
        let bytes = encode(&ClientMessage::Ping { nonce: 42 }).unwrap();
        assert!(matches!(
            decode::<ClientMessage>(&bytes[..bytes.len() - 1]),
            Err(FrameError::Decode(_))
        ));
    }

    #[tokio::test]
    async fn codec_frames_survive_a_stream() {
        let (a, b) = tokio::io::duplex(1024);
        let mut tx = FramedWrite::new(a, codec());
        let mut rx = FramedRead::new(b, codec());
        for nonce in 0..3u64 {
            tx.send(encode(&ClientMessage::Ping { nonce }).unwrap())
                .await
                .unwrap();
        }
        for nonce in 0..3u64 {
            let frame = rx.next().await.unwrap().unwrap();
            assert_eq!(
                decode::<ClientMessage>(&frame).unwrap(),
                ClientMessage::Ping { nonce }
            );
        }
    }

    #[test]
    fn length_prefix_is_u32_little_endian() {
        let mut c = codec();
        let mut buf = bytes::BytesMut::new();
        tokio_util::codec::Encoder::encode(&mut c, bytes::Bytes::from_static(b"ab"), &mut buf)
            .unwrap();
        assert_eq!(&buf[..], &[2, 0, 0, 0, b'a', b'b']);
    }
}
