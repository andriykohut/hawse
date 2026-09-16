use bytes::BytesMut;
use hawse_proto::msg::reset::ABORTED;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::transport::{RecvHalf, SendHalf};

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

/// Ends when both directions have delivered EOF. An abort on either side resets the send stream, so the peer sees a reset rather than a truncated payload delivered as if complete.
pub async fn pump<S>(
    socket: S,
    send: SendHalf,
    mut recv: RecvHalf,
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
                .write_bytes(buf.split().freeze())
                .await
                .map_err(PumpError::Stream)?;
        }
        send.take().finish().await;
        Ok::<u64, PumpError>(total)
    };

    let to_socket = async move {
        let mut total = 0u64;
        while let Some(bytes) = recv.read_bytes(buffer).await.map_err(PumpError::Stream)? {
            total += u64::try_from(bytes.len()).expect("usize fits u64");
            writer.write_all(&bytes).await.map_err(PumpError::Socket)?;
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
