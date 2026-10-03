//! What the TCP transport writes on a yamux stream in place of raw bytes. yamux hands a reset
//! stream to its reader as end-of-stream, so a stream's clean finish is a record of its own and a
//! stream that ends without one reads as reset, whichever way yamux closed it.

use std::fmt;
use std::future::poll_fn;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, Waker, ready};

use bytes::{Buf, BytesMut};
use hawse_proto::record::{self, Head};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// The source of the `ConnectionReset` a `RecordReader` fails with. `code` is `None` when the
/// stream ended without saying why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamReset {
    pub code: Option<u32>,
}

impl fmt::Display for StreamReset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.code {
            Some(code) => write!(f, "the peer reset the stream with code {code:#x}"),
            None => f.write_str("the stream ended without a finish"),
        }
    }
}

impl std::error::Error for StreamReset {}

fn reset_error(code: Option<u32>) -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionReset, StreamReset { code })
}

#[derive(Debug)]
enum Reading {
    /// Collecting a header: `have` bytes of it are in `head`.
    Head {
        head: [u8; record::MAX_HEAD],
        have: usize,
    },
    /// Inside a data record with `left` bytes still to hand on.
    Data {
        left: usize,
    },
    Finished,
    Reset(Option<u32>),
}

impl Reading {
    fn head() -> Self {
        Self::Head {
            head: [0; record::MAX_HEAD],
            have: 0,
        }
    }
}

#[derive(Debug)]
pub struct RecordReader<R> {
    inner: R,
    state: Reading,
}

impl<R> RecordReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            state: Reading::head(),
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for RecordReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            match &mut this.state {
                Reading::Finished => return Poll::Ready(Ok(())),
                Reading::Reset(code) => return Poll::Ready(Err(reset_error(*code))),
                Reading::Data { left } => {
                    if out.remaining() == 0 {
                        return Poll::Ready(Ok(()));
                    }
                    // Straight into the caller's buffer, capped at what this record still holds.
                    let want = (*left).min(out.remaining());
                    let mut window = ReadBuf::new(out.initialize_unfilled_to(want));
                    ready!(Pin::new(&mut this.inner).poll_read(cx, &mut window))?;
                    let n = window.filled().len();
                    if n == 0 {
                        this.state = Reading::Reset(None);
                    } else {
                        out.advance(n);
                        this.state = if *left == n {
                            Reading::head()
                        } else {
                            Reading::Data { left: *left - n }
                        };
                        return Poll::Ready(Ok(()));
                    }
                }
                Reading::Head { head, have } => {
                    // The tag comes first and says how long the rest of the header is.
                    let need = if *have == 0 {
                        Some(1)
                    } else {
                        record::head_len(head[0])
                    };
                    this.state = match need {
                        None => Reading::Reset(None),
                        Some(need) if *have < need => {
                            let mut window = ReadBuf::new(&mut head[*have..need]);
                            ready!(Pin::new(&mut this.inner).poll_read(cx, &mut window))?;
                            match window.filled().len() {
                                0 => Reading::Reset(None),
                                n => Reading::Head {
                                    head: *head,
                                    have: *have + n,
                                },
                            }
                        }
                        Some(need) => match record::decode(&head[..need]) {
                            Some(Head::Data(len)) => Reading::Data {
                                left: usize::from(len),
                            },
                            Some(Head::Finish) => Reading::Finished,
                            Some(Head::Reset(code)) => Reading::Reset(Some(code)),
                            None => Reading::Reset(None),
                        },
                    };
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct RecordWriter<W> {
    inner: W,
    /// Whole records `inner` has not taken yet. Never part of one: a write that is dropped
    /// leaves a stream the reader still parses.
    buf: BytesMut,
    /// A finish or a reset record is queued, and nothing may follow it.
    ended: bool,
}

impl<W> RecordWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            buf: BytesMut::new(),
            ended: false,
        }
    }
}

impl<W: AsyncWrite + Unpin> RecordWriter<W> {
    fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while self.buf.has_remaining() {
            let n = ready!(Pin::new(&mut self.inner).poll_write(cx, &self.buf))?;
            if n == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            self.buf.advance(n);
        }
        Poll::Ready(Ok(()))
    }

    /// Queues the stream's last record, once: a finish does not follow a reset, nor a second one
    /// the first.
    fn end_with(&mut self, last: &[u8]) {
        if !self.ended {
            self.ended = true;
            self.buf.extend_from_slice(last);
        }
    }

    /// One poll each to send what is queued, the reset record, and the shutdown. This runs in
    /// `Drop`s, where nothing can be awaited: when a poll is not ready the rest is skipped, and the
    /// peer still reads a reset, without the code, because no finish record reached it.
    pub fn reset(&mut self, code: u32) {
        self.end_with(&record::reset(code));
        let mut cx = Context::from_waker(Waker::noop());
        if self.poll_drain(&mut cx).is_ready() {
            let _: Poll<io::Result<()>> = Pin::new(&mut self.inner).poll_shutdown(&mut cx);
        }
    }

    /// `reset`, waiting until the code has left.
    pub async fn reset_flushed(&mut self, code: u32) -> io::Result<()> {
        self.end_with(&record::reset(code));
        poll_fn(|cx| self.poll_drain(cx)).await?;
        poll_fn(|cx| Pin::new(&mut self.inner).poll_shutdown(cx)).await
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for RecordWriter<W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.ended {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        // At most one record waits here, so the stream's own back-pressure still reaches the
        // caller, one record late.
        ready!(this.poll_drain(cx))?;
        let n = data.len().min(record::MAX_DATA);
        if n == 0 {
            return Poll::Ready(Ok(0));
        }
        let len = u16::try_from(n).expect("capped at one record");
        this.buf.extend_from_slice(&record::data(len));
        this.buf.extend_from_slice(&data[..n]);
        // One poll now, so a caller that never flushes still sends whenever the stream has room.
        if let Poll::Ready(Err(err)) = this.poll_drain(cx) {
            return Poll::Ready(Err(err));
        }
        Poll::Ready(Ok(n))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.poll_drain(cx))?;
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.end_with(&[record::FINISH]);
        ready!(this.poll_drain(cx))?;
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use proptest::prelude::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

    use super::*;

    fn pipe(capacity: usize) -> (RecordWriter<DuplexStream>, RecordReader<DuplexStream>) {
        let (a, b) = tokio::io::duplex(capacity);
        (RecordWriter::new(a), RecordReader::new(b))
    }

    fn reset_of(err: &io::Error) -> StreamReset {
        assert_eq!(err.kind(), io::ErrorKind::ConnectionReset, "{err:?}");
        *err.get_ref()
            .and_then(|source| source.downcast_ref::<StreamReset>())
            .expect("a StreamReset source")
    }

    #[tokio::test]
    async fn a_finished_stream_reads_to_its_end() {
        let (mut writer, mut reader) = pipe(1024);
        writer.write_all(b"hello").await.unwrap();
        writer.shutdown().await.unwrap();
        let mut got = Vec::new();
        reader.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"hello");
    }

    #[tokio::test]
    async fn a_reset_record_fails_the_read_with_its_code() {
        let (mut writer, mut reader) = pipe(1024);
        writer.write_all(b"hi").await.unwrap();
        writer.reset_flushed(0x42).await.unwrap();
        let mut got = Vec::new();
        let err = reader.read_to_end(&mut got).await.unwrap_err();
        assert_eq!(reset_of(&err).code, Some(0x42));
        assert_eq!(got, b"hi");
    }

    #[tokio::test]
    async fn a_reset_from_a_drop_still_carries_its_code_when_the_pipe_has_room() {
        let (mut writer, mut reader) = pipe(1024);
        writer.write_all(b"hi").await.unwrap();
        writer.reset(0x12);
        drop(writer);
        let mut got = Vec::new();
        let err = reader.read_to_end(&mut got).await.unwrap_err();
        assert_eq!(reset_of(&err).code, Some(0x12));
    }

    #[tokio::test]
    async fn a_stream_that_ends_without_a_finish_reads_as_a_reset() {
        let (mut writer, mut reader) = pipe(1024);
        writer.write_all(b"hi").await.unwrap();
        writer.flush().await.unwrap();
        drop(writer);
        let mut got = Vec::new();
        let err = reader.read_to_end(&mut got).await.unwrap_err();
        assert_eq!(reset_of(&err).code, None);
        assert_eq!(got, b"hi");
    }

    /// Raw bytes on the far end, so the stream can be malformed in ways the writer never produces.
    async fn read_raw(bytes: &[u8]) -> (Vec<u8>, io::Error) {
        let (mut raw, far) = tokio::io::duplex(1024);
        raw.write_all(bytes).await.unwrap();
        drop(raw);
        let mut reader = RecordReader::new(far);
        let mut got = Vec::new();
        let err = reader.read_to_end(&mut got).await.unwrap_err();
        (got, err)
    }

    #[tokio::test]
    async fn a_stream_cut_inside_a_record_reads_as_a_reset() {
        let (got, err) = read_raw(&[record::DATA, 5, 0, b'h', b'i']).await;
        assert_eq!(got, b"hi");
        assert_eq!(reset_of(&err).code, None);
        let (got, err) = read_raw(&[record::RESET, 0x11, 0]).await;
        assert_eq!(got, b"");
        assert_eq!(reset_of(&err).code, None);
    }

    #[tokio::test]
    async fn an_unknown_tag_and_an_empty_data_record_read_as_resets() {
        let (_, err) = read_raw(&[0x7f, 1, 2, 3]).await;
        assert_eq!(reset_of(&err).code, None);
        let (_, err) = read_raw(&[record::DATA, 0, 0, record::FINISH]).await;
        assert_eq!(reset_of(&err).code, None);
    }

    #[tokio::test]
    async fn a_read_that_ended_in_a_reset_keeps_failing() {
        let (mut writer, mut reader) = pipe(1024);
        writer.reset_flushed(0x10).await.unwrap();
        let mut buf = [0u8; 8];
        for _ in 0..2 {
            let err = reader.read(&mut buf).await.unwrap_err();
            assert_eq!(reset_of(&err).code, Some(0x10));
        }
    }

    #[tokio::test]
    async fn a_read_after_the_finish_stays_at_the_end() {
        let (mut writer, mut reader) = pipe(1024);
        writer.shutdown().await.unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(reader.read(&mut buf).await.unwrap(), 0);
        assert_eq!(reader.read(&mut buf).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_write_past_one_record_is_split() {
        let (a, mut raw) = tokio::io::duplex(256 * 1024);
        let mut writer = RecordWriter::new(a);
        writer.write_all(&vec![7u8; 70_000]).await.unwrap();
        writer.shutdown().await.unwrap();
        let mut wire = Vec::new();
        raw.read_to_end(&mut wire).await.unwrap();
        // 65,535 bytes in the first record leave 4,465 for the second.
        assert_eq!(wire[..3], [record::DATA, 0xff, 0xff]);
        let second = 3 + 65_535;
        assert_eq!(wire[second..second + 3], record::data(4_465));
        assert_eq!(wire.last(), Some(&record::FINISH));
        assert_eq!(wire.len(), 3 + 65_535 + 3 + 4_465 + 1);
    }

    #[tokio::test]
    async fn a_write_after_the_end_fails() {
        let (mut writer, _reader) = pipe(1024);
        writer.shutdown().await.unwrap();
        let err = writer.write_all(b"late").await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);

        let (mut writer, _reader) = pipe(1024);
        writer.reset(0x12);
        let err = writer.write_all(b"late").await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }

    /// The second write cannot start until the pipe takes the first record, so it is still
    /// pending when it is dropped, having accepted nothing.
    #[tokio::test]
    async fn a_write_dropped_midway_leaves_a_stream_the_reader_parses() {
        let (mut writer, mut reader) = pipe(16);
        writer.write_all(&[1u8; 100]).await.unwrap();
        let dropped =
            tokio::time::timeout(Duration::from_millis(20), writer.write_all(&[2u8; 100])).await;
        assert!(
            dropped.is_err(),
            "the pipe was full, so the write had to wait"
        );

        let reading = tokio::spawn(async move {
            let mut got = Vec::new();
            reader.read_to_end(&mut got).await.map(|_| got)
        });
        writer.write_all(b"tail").await.unwrap();
        writer.shutdown().await.unwrap();
        let mut expected = vec![1u8; 100];
        expected.extend_from_slice(b"tail");
        assert_eq!(reading.await.unwrap().unwrap(), expected);
    }

    /// `write_all` returns as soon as the record is accepted; it is the flush that waits for the
    /// pipe, and so the flush that every sender of a last record has to make.
    #[tokio::test]
    async fn a_record_larger_than_the_pipe_arrives_once_the_reader_catches_up() {
        let (mut writer, mut reader) = pipe(64);
        writer.write_all(&[7u8; 1000]).await.unwrap();
        let flushing = tokio::spawn(async move {
            writer.flush().await.unwrap();
            writer
        });
        let mut got = vec![0u8; 1000];
        tokio::time::timeout(Duration::from_secs(5), reader.read_exact(&mut got))
            .await
            .expect("the record arrived")
            .unwrap();
        assert_eq!(got, vec![7u8; 1000]);
        let _writer = flushing.await.unwrap();
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// A one-byte pipe hands the reader every header one byte at a time.
        #[test]
        fn chunks_arrive_intact_through_any_pipe_and_any_read_size(
            chunks in proptest::collection::vec(
                proptest::collection::vec(any::<u8>(), 0..3000),
                0..8,
            ),
            capacity in 1usize..256,
            read_size in 1usize..512,
        ) {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let expected: Vec<u8> = chunks.concat();
            let got = runtime.block_on(async move {
                let (mut writer, mut reader) = pipe(capacity);
                let writing = tokio::spawn(async move {
                    for chunk in chunks {
                        writer.write_all(&chunk).await.unwrap();
                    }
                    writer.shutdown().await.unwrap();
                });
                let mut got = Vec::new();
                let mut buf = vec![0u8; read_size];
                loop {
                    let n = reader.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    got.extend_from_slice(&buf[..n]);
                }
                writing.await.unwrap();
                got
            });
            prop_assert_eq!(got, expected);
        }
    }
}
