use bytes::BytesMut;
use quinn::{RecvStream, SendStream, VarInt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub to_stream: u64,
    pub to_socket: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum PumpError {
    #[error("socket failed")]
    Socket(#[source] std::io::Error),
    #[error("stream write failed")]
    Write(#[source] quinn::WriteError),
    #[error("stream read failed")]
    Read(#[source] quinn::ReadError),
}

// quinn's own `Drop for SendStream` finishes (not resets) a stream that's dropped mid-transfer,
// so try_join! cancelling this half on the other side's error would otherwise hand the peer a
// clean end-of-stream on a truncated payload.
const RESET_ABORTED: u32 = 0x12;

struct ResetOnDrop(Option<SendStream>);

impl ResetOnDrop {
    fn stream(&mut self) -> &mut SendStream {
        self.0.as_mut().expect("not yet taken")
    }

    fn take(mut self) -> SendStream {
        self.0.take().expect("not yet taken")
    }
}

impl Drop for ResetOnDrop {
    fn drop(&mut self) {
        if let Some(mut send) = self.0.take() {
            let _: Result<(), quinn::ClosedStream> = send.reset(VarInt::from_u32(RESET_ABORTED));
        }
    }
}

/// Ends when both directions have delivered EOF. An abort on either side resets the send stream, so the peer sees a reset rather than a truncated payload delivered as if complete.
pub async fn pump<S>(
    socket: S,
    send: SendStream,
    mut recv: RecvStream,
    buffer: usize,
) -> Result<Stats, PumpError>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(socket);

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
                .write_chunk(buf.split().freeze())
                .await
                .map_err(PumpError::Write)?;
        }
        let _: Result<(), quinn::ClosedStream> = send.take().finish();
        Ok::<u64, PumpError>(total)
    };

    let to_socket = async move {
        let mut total = 0u64;
        while let Some(chunk) = recv
            .read_chunk(buffer, true)
            .await
            .map_err(PumpError::Read)?
        {
            total += u64::try_from(chunk.bytes.len()).expect("usize fits u64");
            writer
                .write_all(&chunk.bytes)
                .await
                .map_err(PumpError::Socket)?;
        }
        let _ = writer.shutdown().await;
        Ok::<u64, PumpError>(total)
    };

    let (to_stream, to_socket) = tokio::try_join!(to_stream, to_socket)?;
    Ok(Stats {
        to_stream,
        to_socket,
    })
}
