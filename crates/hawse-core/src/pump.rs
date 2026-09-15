use bytes::BytesMut;
use quinn::{RecvStream, SendStream};
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

/// Ends when both directions have delivered EOF. An error on either side aborts both, which drops the stream unfinished and resets it for the peer.
pub async fn pump<S>(
    socket: S,
    mut send: SendStream,
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
        loop {
            buf.reserve(buffer);
            let n = reader.read_buf(&mut buf).await.map_err(PumpError::Socket)?;
            if n == 0 {
                break;
            }
            total += u64::try_from(n).unwrap_or(u64::MAX);
            send.write_chunk(buf.split().freeze())
                .await
                .map_err(PumpError::Write)?;
        }
        let _: Result<(), quinn::ClosedStream> = send.finish();
        Ok::<u64, PumpError>(total)
    };

    let to_socket = async move {
        let mut total = 0u64;
        while let Some(chunk) = recv
            .read_chunk(buffer, true)
            .await
            .map_err(PumpError::Read)?
        {
            total += u64::try_from(chunk.bytes.len()).unwrap_or(u64::MAX);
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
